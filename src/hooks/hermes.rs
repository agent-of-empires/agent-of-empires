//! Hermes's `config.yaml` hooks plus its shell-hook consent allowlist.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use serde_yaml::{Mapping, Value as Yaml};

use crate::agents::ResolvedHookEvent;

use super::command::{hook_command, is_aoe_hook_command};
use super::config_io::{log_installed, log_removed, log_unchanged, with_config_lock, write_file};
use super::HookInstallTarget;

fn yaml_command_is_aoe(hook: &Yaml) -> bool {
    hook.as_mapping()
        .and_then(|m| m.get(Yaml::String("command".into())))
        .and_then(Yaml::as_str)
        .is_some_and(is_aoe_hook_command)
}

/// Install AoE hooks into `config.yaml` and pre-approve them in
/// `shell-hooks-allowlist.json` so Hermes does not prompt for consent. The two
/// writes are not atomic together; a stale allowlist only re-prompts.
/// Unchanged files are not rewritten.
pub fn install_hermes_hooks_with_events(
    config_path: &Path,
    target: HookInstallTarget,
    events: &[ResolvedHookEvent],
) -> Result<()> {
    if events.is_empty() && !config_path.exists() {
        return Ok(());
    }
    with_config_lock(config_path, "yaml.lock", || {
        let content = if config_path.exists() {
            std::fs::read_to_string(config_path)?
        } else {
            String::new()
        };
        let mut config = if content.trim().is_empty() {
            Yaml::Mapping(Mapping::new())
        } else {
            serde_yaml::from_str(&content)
                .with_context(|| format!("Failed to parse {}", config_path.display()))?
        };
        let yaml_before = config.clone();

        let root = config
            .as_mapping_mut()
            .ok_or_else(|| anyhow::anyhow!("Hermes config root is not a YAML mapping"))?;
        let hooks = root
            .entry(Yaml::String("hooks".to_string()))
            .or_insert_with(|| Yaml::Mapping(Mapping::new()));
        if !hooks.is_mapping() {
            *hooks = Yaml::Mapping(Mapping::new());
        }
        let hooks = hooks.as_mapping_mut().expect("ensured mapping above");
        for event in events {
            let Some(status) = event.status else {
                continue;
            };
            let entries = hooks
                .entry(Yaml::String(event.name.clone()))
                .or_insert_with(|| Yaml::Sequence(Vec::new()));
            if !entries.is_sequence() {
                *entries = Yaml::Sequence(Vec::new());
            }
            let entries = entries.as_sequence_mut().expect("ensured sequence above");
            entries.retain(|hook| !yaml_command_is_aoe(hook));
            let mut entry = Mapping::new();
            entry.insert(
                Yaml::String("command".into()),
                Yaml::String(hook_command(status.as_str(), target)),
            );
            entries.push(Yaml::Mapping(entry));
        }
        let yaml_changed = config != yaml_before;

        let config_dir = config_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;
        let (allowlist_path, allowlist) = render_hermes_allowlist(config_dir, target, events)?;
        // Rendering keeps `approved_at` and is deterministic, so bytes compare.
        let allowlist_changed =
            !allowlist_path.exists() || std::fs::read(&allowlist_path)? != allowlist.as_bytes();

        if !yaml_changed && !allowlist_changed {
            log_unchanged(config_path);
            return Ok(());
        }
        if yaml_changed {
            write_file(config_path, serde_yaml::to_string(&config)?.as_bytes())?;
        }
        if allowlist_changed {
            write_file(&allowlist_path, allowlist.as_bytes())?;
        }
        log_installed(config_path);
        Ok(())
    })
}

pub fn uninstall_hermes_hooks(config_path: &Path) -> Result<bool> {
    if !config_path.exists() {
        return Ok(false);
    }
    with_config_lock(config_path, "yaml.lock", || {
        let content = std::fs::read_to_string(config_path)?;
        if content.trim().is_empty() {
            return Ok(false);
        }
        let mut config: Yaml = serde_yaml::from_str(&content)
            .with_context(|| format!("Failed to parse {}", config_path.display()))?;
        let hooks_key = Yaml::String("hooks".to_string());
        let Some(root) = config.as_mapping_mut() else {
            return Ok(false);
        };
        let Some(hooks) = root.get_mut(&hooks_key).and_then(Yaml::as_mapping_mut) else {
            return Ok(false);
        };

        let mut modified = false;
        for entries in hooks.values_mut().filter_map(Yaml::as_sequence_mut) {
            let before = entries.len();
            entries.retain(|hook| !yaml_command_is_aoe(hook));
            modified |= entries.len() != before;
        }
        if !modified {
            return Ok(false);
        }
        let empty: Vec<Yaml> = hooks
            .iter()
            .filter(|(_, v)| v.as_sequence().is_some_and(Vec::is_empty))
            .map(|(k, _)| k.clone())
            .collect();
        for key in empty {
            hooks.remove(&key);
        }
        if hooks.is_empty() {
            root.remove(&hooks_key);
        }

        crate::session::atomic_write(config_path, serde_yaml::to_string(&config)?.as_bytes())?;
        log_removed(config_path);
        Ok(true)
    })
}

/// Render the allowlist with one approval per installed `(event, command)`.
/// An existing pair keeps its `approved_at` (even `null`), so a reinstall of
/// current commands is byte-identical.
fn render_hermes_allowlist(
    config_dir: &Path,
    target: HookInstallTarget,
    events: &[ResolvedHookEvent],
) -> Result<(PathBuf, String)> {
    let allowlist_path = config_dir.join("shell-hooks-allowlist.json");
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let mut data: Value = if allowlist_path.exists() {
        serde_json::from_str(&std::fs::read_to_string(&allowlist_path)?)
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
        let same = |entry: &Value| {
            entry.get("event").and_then(Value::as_str) == Some(event.name.as_str())
                && entry.get("command").and_then(Value::as_str) == Some(&cmd)
        };
        let preserved = approvals.iter().find_map(|entry| {
            same(entry)
                .then(|| entry.get("approved_at").cloned())
                .flatten()
        });
        approvals.retain(|entry| !same(entry));
        approvals.push(serde_json::json!({
            "event": event.name,
            "command": cmd,
            "approved_at": preserved.unwrap_or_else(|| Value::String(now.clone())),
            "script_mtime_at_approval": Value::Null,
        }));
    }

    Ok((allowlist_path, serde_json::to_string_pretty(&data)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::HookStatus;
    use crate::hooks::test_support::{agent_events, read_json};
    use tempfile::TempDir;

    fn install(path: &Path) -> Result<()> {
        install_hermes_hooks_with_events(
            path,
            HookInstallTarget::Host,
            &agent_events("hermes", &[]),
        )
    }

    fn read_yaml(path: &Path) -> Yaml {
        serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn hermes_hooks_install_beside_user_config_and_uninstall() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.yaml");
        std::fs::write(
            &path,
            "model: hermes-pro\nhooks:\n  pre_tool_call:\n    - command: \"echo user-hook\"\n      matcher: \"terminal\"\nhooks_auto_accept: false\n",
        )
        .unwrap();
        let allowlist_path = tmp.path().join("shell-hooks-allowlist.json");
        std::fs::write(&allowlist_path, "{\"version\":7,\"approvals\":[]}").unwrap();

        install(&path).unwrap();

        let config = read_yaml(&path);
        assert_eq!(config["model"].as_str(), Some("hermes-pro"));
        assert_eq!(config["hooks_auto_accept"].as_bool(), Some(false));
        let events = agent_events("hermes", &[]);
        for event in &events {
            let entries = config["hooks"][event.name.as_str()].as_sequence().unwrap();
            assert_eq!(
                entries.iter().filter(|h| yaml_command_is_aoe(h)).count(),
                1,
                "{}",
                event.name
            );
            assert!(yaml_command_is_aoe(entries.last().unwrap()));
        }
        assert_eq!(
            config["hooks"]["pre_tool_call"][0]["command"].as_str(),
            Some("echo user-hook")
        );
        let allowlist = read_json(&allowlist_path);
        assert_eq!(allowlist["version"], 7);
        assert_eq!(
            allowlist["approvals"].as_array().unwrap().len(),
            events.len()
        );

        assert!(uninstall_hermes_hooks(&path).unwrap());
        let config = read_yaml(&path);
        let pre_tool = config["hooks"]["pre_tool_call"].as_sequence().unwrap();
        assert_eq!(pre_tool.len(), 1);
        assert_eq!(pre_tool[0]["command"].as_str(), Some("echo user-hook"));
        assert!(config["hooks"].get("post_llm_call").is_none());
        assert!(!uninstall_hermes_hooks(&tmp.path().join("missing.yaml")).unwrap());
    }

    #[test]
    fn hermes_install_uses_status_override_in_both_files() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.yaml");
        let events = agent_events("hermes", &[("pre_approval_request", HookStatus::Error)]);
        install_hermes_hooks_with_events(&path, HookInstallTarget::Host, &events).unwrap();

        let cmd = read_yaml(&path)["hooks"]["pre_approval_request"][0]["command"].clone();
        assert!(cmd.as_str().unwrap().contains("printf error"));
        let allowlist = read_json(&tmp.path().join("shell-hooks-allowlist.json"));
        let approval = allowlist["approvals"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["event"] == "pre_approval_request")
            .unwrap();
        assert!(approval["command"]
            .as_str()
            .unwrap()
            .contains("printf error"));
    }

    #[test]
    fn hermes_install_rejects_invalid_files_without_writing() {
        for (config, allowlist) in [
            ("hooks:\n  pre_tool_call: [\n", None),
            ("model: claude-opus\n", Some("{ invalid json")),
        ] {
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join("config.yaml");
            let allowlist_path = tmp.path().join("shell-hooks-allowlist.json");
            std::fs::write(&path, config).unwrap();
            if let Some(allowlist) = allowlist {
                std::fs::write(&allowlist_path, allowlist).unwrap();
            }

            assert!(install(&path).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), config);
            assert_eq!(
                std::fs::read_to_string(&allowlist_path).ok().as_deref(),
                allowlist
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn hermes_install_preserves_symlinked_files() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".hermes")).unwrap();
        std::fs::create_dir_all(tmp.path().join("dotfiles")).unwrap();
        let config_target = tmp.path().join("dotfiles/hermes-config.yaml");
        let allowlist_target = tmp.path().join("dotfiles/hermes-allowlist.json");
        std::fs::write(&config_target, "user_field: keep-me\n").unwrap();
        std::fs::write(&allowlist_target, "{\"approvals\":[]}\n").unwrap();
        let config_path = tmp.path().join(".hermes/config.yaml");
        let allowlist_path = tmp.path().join(".hermes/shell-hooks-allowlist.json");
        std::os::unix::fs::symlink("../dotfiles/hermes-config.yaml", &config_path).unwrap();
        std::os::unix::fs::symlink("../dotfiles/hermes-allowlist.json", &allowlist_path).unwrap();
        let is_link = |p: &Path| {
            std::fs::symlink_metadata(p)
                .unwrap()
                .file_type()
                .is_symlink()
        };

        install(&config_path).unwrap();
        assert!(is_link(&config_path) && is_link(&allowlist_path));
        let config = read_yaml(&config_target);
        assert_eq!(config["user_field"].as_str(), Some("keep-me"));
        let events = agent_events("hermes", &[]);
        for event in &events {
            assert!(
                config["hooks"].get(event.name.as_str()).is_some(),
                "{}",
                event.name
            );
        }
        assert_eq!(
            read_json(&allowlist_target)["approvals"]
                .as_array()
                .unwrap()
                .len(),
            events.len()
        );

        uninstall_hermes_hooks(&config_path).unwrap();
        assert!(is_link(&config_path) && is_link(&allowlist_path));
        let config = read_yaml(&config_target);
        assert_eq!(config["user_field"].as_str(), Some("keep-me"));
        assert!(config.get("hooks").is_none());
    }
}
