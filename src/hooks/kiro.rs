//! Kiro CLI agent configs: flat `hooks.<event>[].command` in an agent JSON file.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Map, Value};

use crate::agents::ResolvedHookEvent;

use super::command::{hook_command, hook_command_session_id, json_command_is_aoe};
use super::config_io::{log_installed, log_removed, log_unchanged, with_config_lock, write_json};
use super::json_settings::{drop_empty_events, remove_flat_aoe_entries};
use super::HookInstallTarget;

/// The dedicated agent's name, and the stem of [`KIRO_HOOKS_AGENT_FILE`].
/// Renaming it is user-visible, so it is not tied to the hook marker.
macro_rules! kiro_hooks_agent_name {
    () => {
        "aoe-hooks"
    };
}

const KIRO_HOOKS_AGENT_NAME: &str = kiro_hooks_agent_name!();

/// Home-relative config of the dedicated agent AoE installs into, leaving the
/// user's default agent untouched.
pub const KIRO_HOOKS_AGENT_FILE: &str = concat!(".kiro/agents/", kiro_hooks_agent_name!(), ".json");

fn read_agent_config(path: &Path) -> Result<Map<String, Value>> {
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?).unwrap_or_default())
}

/// Install AoE hooks into a Kiro agent config, creating it with a default
/// name and tools when absent. Pure file IO; see
/// [`set_kiro_default_agent_if_builtin`] to activate the dedicated agent.
pub fn install_kiro_hooks_with_events(
    agent_config_path: &Path,
    target: HookInstallTarget,
    events: &[ResolvedHookEvent],
) -> Result<()> {
    if events.is_empty() && !agent_config_path.exists() {
        return Ok(());
    }
    with_config_lock(agent_config_path, "json.lock", || {
        let mut config = if agent_config_path.exists() {
            read_agent_config(agent_config_path)?
        } else {
            Map::new()
        };
        let before = config.clone();

        let default_name = agent_config_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(KIRO_HOOKS_AGENT_NAME);
        config
            .entry("name")
            .or_insert_with(|| Value::String(default_name.to_string()));
        config
            .entry("tools")
            .or_insert_with(|| serde_json::json!(["*"]));

        let mut hooks = config
            .get("hooks")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for event in events {
            // Kiro has no stdin tool gate, so the plain status writer is used.
            let identity = event
                .identity_field
                .map(|field| hook_command_session_id(target, field));
            let status = event
                .status
                .map(|status| hook_command(status.as_str(), target));
            let commands: Vec<String> = identity.into_iter().chain(status).collect();
            if commands.is_empty() {
                continue;
            }
            if let Some(entries) = hooks
                .entry(event.name.clone())
                .or_insert_with(|| Value::Array(Vec::new()))
                .as_array_mut()
            {
                entries.retain(|hook| !json_command_is_aoe(hook));
                entries.extend(
                    commands
                        .into_iter()
                        .map(|command| serde_json::json!({ "command": command })),
                );
            }
        }
        config.insert("hooks".to_string(), Value::Object(hooks));

        if config == before {
            log_unchanged(agent_config_path);
            return Ok(());
        }
        write_json(agent_config_path, &Value::Object(config))?;
        log_installed(agent_config_path);
        Ok(())
    })
}

/// The file under `agents_dir` Kiro loads for `--agent <name>`.
///
/// Kiro matches the `name` field, not the filename (generators write
/// `<prefix>-<name>.json`), so installing by stem would miss the loaded file.
/// Falls back to `<name>.json`, the create path for a new agent; the caller
/// has already rejected separators and `..` in `name`.
pub fn resolve_kiro_agent_file(agents_dir: &Path, name: &str) -> PathBuf {
    find_kiro_agent_file_by_name(agents_dir, name)
        .unwrap_or_else(|| agents_dir.join(format!("{name}.json")))
}

/// First `*.json` (sorted) whose `name` matches; unreadable files are skipped.
/// Duplicates are a user misconfiguration and are warned about.
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
            .is_some_and(|v| v.get("name").and_then(Value::as_str) == Some(name))
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

/// Make `aoe-hooks` the default Kiro agent while the user is still on Kiro's
/// built-in default. Best-effort: failures are logged and ignored.
pub fn set_kiro_default_agent_if_builtin() {
    let current_default = std::process::Command::new("kiro-cli")
        .args(["settings", "chat.defaultAgent", "--format", "json"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    // `--format json` prints `null` when unset and a quoted name otherwise.
    let trimmed = current_default.trim();
    if !(trimmed.is_empty() || trimmed == "null" || trimmed == "\"kiro_default\"") {
        tracing::info!(target: "hooks.install",
            "Kiro has a custom default agent; skipping set-default. \
             Run `kiro-cli agent set-default {KIRO_HOOKS_AGENT_NAME}` to enable status detection."
        );
        return;
    }
    match std::process::Command::new("kiro-cli")
        .args(["agent", "set-default", KIRO_HOOKS_AGENT_NAME])
        .output()
    {
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
}

/// Remove AoE hooks from a Kiro agent config, deleting a file left empty.
pub fn uninstall_kiro_hooks(agent_config_path: &Path) -> Result<bool> {
    if !agent_config_path.exists() {
        return Ok(false);
    }
    with_config_lock(agent_config_path, "json.lock", || {
        let mut config = read_agent_config(agent_config_path)?;
        let Some(hooks) = config.get_mut("hooks").and_then(Value::as_object_mut) else {
            return Ok(false);
        };
        if !remove_flat_aoe_entries(hooks) {
            return Ok(false);
        }
        drop_empty_events(hooks);
        if hooks.is_empty() {
            config.remove("hooks");
        }

        if config.is_empty() {
            std::fs::remove_file(agent_config_path)?;
        } else {
            crate::session::atomic_write(
                agent_config_path,
                serde_json::to_string_pretty(&Value::Object(config))?.as_bytes(),
            )?;
        }
        log_removed(agent_config_path);
        Ok(true)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::HookStatus;
    use crate::hooks::test_support::{agent_events, read_json};
    use tempfile::TempDir;

    fn install(path: &Path) {
        install_kiro_hooks_with_events(path, HookInstallTarget::Host, &agent_events("kiro", &[]))
            .unwrap();
    }

    fn assert_aoe_hooks_installed(config: &Value) {
        for event in agent_events("kiro", &[]) {
            let entries = config["hooks"][event.name.as_str()].as_array().unwrap();
            let aoe = entries.iter().filter(|e| json_command_is_aoe(e)).count();
            assert_eq!(
                aoe,
                1 + usize::from(event.identity_field.is_some()),
                "{}",
                event.name
            );
        }
    }

    #[test]
    fn resolve_kiro_agent_file_matches_name_field() {
        /// (case name, files to seed, expected file name)
        type Case = (
            &'static str,
            &'static [(&'static str, &'static str)],
            &'static str,
        );
        let cases: [Case; 5] = [
            ("dir absent", &[], "custom-agent.json"),
            (
                "no match",
                &[("something-else.json", r#"{"name":"something-else"}"#)],
                "custom-agent.json",
            ),
            (
                "name beats stem",
                &[
                    ("TeamAgents-custom-agent.json", r#"{"name":"custom-agent"}"#),
                    ("custom-agent.json", r#"{"name":"aoe-hooks"}"#),
                ],
                "TeamAgents-custom-agent.json",
            ),
            (
                "skips unreadable",
                &[
                    ("notes.txt", "name: custom-agent"),
                    ("broken.json", "{ not json"),
                    ("AcmePkg-custom-agent.json", r#"{"name":"custom-agent"}"#),
                ],
                "AcmePkg-custom-agent.json",
            ),
            (
                "duplicates pick lexicographic first",
                &[
                    ("ZZZ-custom.json", r#"{"name":"custom-agent"}"#),
                    ("AAA-custom.json", r#"{"name":"custom-agent"}"#),
                ],
                "AAA-custom.json",
            ),
        ];
        for (label, files, want) in cases {
            let tmp = TempDir::new().unwrap();
            let dir = tmp.path().join("agents");
            for (name, content) in files {
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join(name), content).unwrap();
            }
            assert_eq!(
                resolve_kiro_agent_file(&dir, "custom-agent"),
                dir.join(want),
                "{label}"
            );
        }
    }

    #[test]
    fn kiro_hooks_create_selected_agent_file_and_uninstall() {
        let tmp = TempDir::new().unwrap();
        let path = resolve_kiro_agent_file(tmp.path(), "custom-agent");
        assert!(!uninstall_kiro_hooks(&path).unwrap());
        install(&path);

        let config = read_json(&path);
        assert_eq!(
            config["name"], "custom-agent",
            "Kiro must load it for --agent custom-agent"
        );
        assert_aoe_hooks_installed(&config);

        assert!(uninstall_kiro_hooks(&path).unwrap());
        assert!(read_json(&path).get("hooks").is_none(), "name/tools remain");
    }

    #[test]
    fn kiro_hooks_preserve_user_agent_config() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("CustomPkg-custom-agent.json");
        std::fs::write(
            &path,
            r#"{"name":"custom-agent","description":"keep me","version":3,"prompt":"custom helper","tools":["read","shell"],"hooks":{"preToolUse":[{"command":"echo mine","matcher":"shell"}]}}"#,
        )
        .unwrap();

        install(&path);
        let config = read_json(&path);
        assert_eq!(config["name"], "custom-agent");
        assert_eq!(config["description"], "keep me");
        assert_eq!(config["version"], 3);
        assert_eq!(config["prompt"], "custom helper");
        assert_eq!(config["tools"].as_array().unwrap().len(), 2);
        assert_eq!(config["hooks"]["preToolUse"][0]["command"], "echo mine");
        assert_aoe_hooks_installed(&config);

        assert!(uninstall_kiro_hooks(&path).unwrap());
        let pre_tool = read_json(&path)["hooks"]["preToolUse"].clone();
        assert_eq!(pre_tool.as_array().unwrap().len(), 1);
        assert_eq!(pre_tool[0]["command"], "echo mine");
    }

    #[test]
    fn kiro_install_uses_status_override() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("aoe-hooks.json");
        let events = agent_events("kiro", &[("stop", HookStatus::Error)]);
        install_kiro_hooks_with_events(&path, HookInstallTarget::Host, &events).unwrap();
        let cmd = read_json(&path)["hooks"]["stop"][0]["command"].clone();
        assert!(cmd.as_str().unwrap().contains("printf error"), "{cmd}");
    }
}
