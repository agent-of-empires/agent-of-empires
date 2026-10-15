//! settl's `config.toml`: a flat `[[hooks]]` array of `{ event, command }`.

use std::path::Path;

use anyhow::Result;

use crate::agents::ResolvedHookEvent;

use super::command::{hook_command, is_aoe_hook_command};
use super::config_io::{log_installed, log_removed, log_unchanged, with_config_lock, write_file};
use super::HookInstallTarget;

fn toml_command_is_aoe(hook: &toml::Value) -> bool {
    hook.get("command")
        .and_then(toml::Value::as_str)
        .is_some_and(is_aoe_hook_command)
}

fn empty_table() -> toml::Value {
    toml::Value::Table(toml::map::Map::new())
}

/// Replace AoE entries in settl's `[[hooks]]`, keeping user hooks. Unchanged
/// content is not rewritten.
pub fn install_settl_hooks_with_events(
    config_path: &Path,
    target: HookInstallTarget,
    events: &[ResolvedHookEvent],
) -> Result<()> {
    if events.is_empty() && !config_path.exists() {
        return Ok(());
    }
    with_config_lock(config_path, "toml.lock", || {
        let mut config = if config_path.exists() {
            toml::from_str(&std::fs::read_to_string(config_path)?).unwrap_or_else(|e| {
                tracing::warn!(target: "hooks.install", "Failed to parse {}: {}", config_path.display(), e);
                empty_table()
            })
        } else {
            empty_table()
        };
        let before = config.clone();

        let hooks = config
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("Config root is not a TOML table"))?
            .entry("hooks")
            .or_insert_with(|| toml::Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("hooks key is not a TOML array"))?;
        hooks.retain(|hook| !toml_command_is_aoe(hook));
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
            hooks.push(toml::Value::Table(entry));
        }

        if config == before {
            log_unchanged(config_path);
            return Ok(());
        }
        write_file(config_path, toml::to_string_pretty(&config)?.as_bytes())?;
        log_installed(config_path);
        Ok(())
    })
}

pub fn uninstall_settl_hooks(config_path: &Path) -> Result<bool> {
    if !config_path.exists() {
        return Ok(false);
    }
    with_config_lock(config_path, "toml.lock", || {
        let content = std::fs::read_to_string(config_path)?;
        let mut config = toml::from_str(&content).unwrap_or_else(|e| {
            tracing::warn!(target: "hooks.uninstall", "Failed to parse {}: {}", config_path.display(), e);
            empty_table()
        });
        let Some(hooks) = config.get_mut("hooks").and_then(toml::Value::as_array_mut) else {
            return Ok(false);
        };
        let before = hooks.len();
        hooks.retain(|hook| !toml_command_is_aoe(hook));
        if hooks.len() == before {
            return Ok(false);
        }
        crate::session::atomic_write(config_path, toml::to_string_pretty(&config)?.as_bytes())?;
        log_removed(config_path);
        Ok(true)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::agent_events;
    use tempfile::TempDir;

    fn hooks(path: &Path) -> Vec<toml::Value> {
        let config: toml::Value = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        config["hooks"].as_array().unwrap().clone()
    }

    #[test]
    fn settl_hooks_install_beside_user_hooks_and_uninstall() {
        let tmp = TempDir::new().unwrap();
        let events = agent_events("settl", &[]);
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "model = \"settl-pro\"\n\n[[hooks]]\nevent = \"GameWon\"\ncommand = \"echo user-hook\"\n").unwrap();

        install_settl_hooks_with_events(&path, HookInstallTarget::Host, &events).unwrap();
        let installed = hooks(&path);
        let names: Vec<_> = installed[1..]
            .iter()
            .map(|h| h["event"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["TurnStarted", "WaitingForHuman", "GameWon"]);
        assert_eq!(installed[0]["command"].as_str(), Some("echo user-hook"));
        assert!(installed[1..].iter().all(toml_command_is_aoe));
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("model = \"settl-pro\""));

        assert!(uninstall_settl_hooks(&path).unwrap());
        let remaining = hooks(&path);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0]["command"].as_str(), Some("echo user-hook"));

        let fresh = tmp.path().join(".settl/config.toml");
        install_settl_hooks_with_events(&fresh, HookInstallTarget::Host, &events).unwrap();
        assert!(uninstall_settl_hooks(&fresh).unwrap());
        assert!(hooks(&fresh).is_empty());
    }
}
