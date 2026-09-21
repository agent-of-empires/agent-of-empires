//! Kimi Code's `config.toml`: settl's `[[hooks]]` shape in a file that also
//! holds provider, model and OAuth settings, so edits go through `toml_edit`
//! to keep the rest of the document byte-stable.

use std::path::Path;

use anyhow::{Context, Result};
use toml_edit::{ArrayOfTables, DocumentMut, Item};

use crate::agents::ResolvedHookEvent;

use super::command::{hook_command, is_aoe_hook_command};
use super::config_io::{log_installed, log_removed, log_unchanged, with_config_lock, write_file};
use super::HookInstallTarget;

fn remove_aoe_hooks(hooks: &mut ArrayOfTables) {
    hooks.retain(|entry| {
        !entry
            .get("command")
            .and_then(Item::as_str)
            .is_some_and(is_aoe_hook_command)
    });
}

pub fn install_kimi_hooks_with_events(
    config_path: &Path,
    target: HookInstallTarget,
    events: &[ResolvedHookEvent],
) -> Result<()> {
    if events.is_empty() && !config_path.exists() {
        return Ok(());
    }
    with_config_lock(config_path, "toml.lock", || {
        let mut config = if config_path.exists() {
            std::fs::read_to_string(config_path)?
                .parse::<DocumentMut>()
                .with_context(|| format!("Failed to parse {}", config_path.display()))?
        } else {
            DocumentMut::new()
        };
        let before = config.to_string();

        let hooks = ensure_kimi_hooks_array(&mut config)?;
        remove_aoe_hooks(hooks);
        for event in events {
            let Some(status) = event.status else {
                continue;
            };
            let mut entry = toml_edit::Table::new();
            entry.insert("event", toml_edit::value(event.name.as_str()));
            entry.insert(
                "command",
                toml_edit::value(hook_command(status.as_str(), target)),
            );
            hooks.push(entry);
        }

        if config.to_string() == before {
            log_unchanged(config_path);
            return Ok(());
        }
        write_file(config_path, config.to_string().as_bytes())?;
        log_installed(config_path);
        Ok(())
    })
}

/// The top-level `hooks` as an array of tables, creating it or converting an
/// inline `hooks = [{...}]` (which Kimi accepts). Any other shape is refused
/// before the document is touched.
fn ensure_kimi_hooks_array(config: &mut DocumentMut) -> Result<&mut ArrayOfTables> {
    let item = config
        .as_table_mut()
        .entry("hooks")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    if let Some(array) = item.as_array() {
        let mut migrated = ArrayOfTables::new();
        for value in array.iter() {
            let inline = value.as_inline_table().ok_or_else(|| {
                anyhow::anyhow!(
                    "Kimi hooks inline array has a non-table entry; leaving config untouched"
                )
            })?;
            let mut table = toml_edit::Table::new();
            for (key, val) in inline.iter() {
                table.insert(key, toml_edit::value(val.clone()));
            }
            migrated.push(table);
        }
        *item = Item::ArrayOfTables(migrated);
    }
    item.as_array_of_tables_mut()
        .ok_or_else(|| anyhow::anyhow!("Kimi hooks key is not an array of tables"))
}

/// Remove AoE hooks from Kimi's `config.toml`, dropping an emptied `hooks`.
pub fn uninstall_kimi_hooks(config_path: &Path) -> Result<bool> {
    if !config_path.exists() {
        return Ok(false);
    }
    with_config_lock(config_path, "toml.lock", || {
        let content = std::fs::read_to_string(config_path)?;
        let mut config = content.parse::<DocumentMut>().unwrap_or_else(|e| {
            tracing::warn!(target: "hooks.uninstall", "Failed to parse {}: {}", config_path.display(), e);
            DocumentMut::new()
        });
        let Some(hooks) = config
            .as_table_mut()
            .get_mut("hooks")
            .and_then(Item::as_array_of_tables_mut)
        else {
            return Ok(false);
        };
        let before = hooks.len();
        remove_aoe_hooks(hooks);
        if hooks.len() == before {
            return Ok(false);
        }
        if hooks.is_empty() {
            config.as_table_mut().remove("hooks");
        }
        crate::session::atomic_write(config_path, config.to_string().as_bytes())?;
        log_removed(config_path);
        Ok(true)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::agent_events;
    use tempfile::TempDir;

    fn install(path: &Path) -> Result<()> {
        install_kimi_hooks_with_events(path, HookInstallTarget::Host, &agent_events("kimi", &[]))
    }

    fn parsed(path: &Path) -> toml::Value {
        toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn kimi_hooks_preserve_surrounding_document() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let original = "\
# ~/.kimi-code/config.toml
default_model = \"kimi-k2\"

[providers.kimi]
type = \"kimi\"
oauth = { storage = \"keyring\", key = \"user@example.com\" }

[models.\"kimi-k2\"]
provider = \"kimi\"
max_context_size = 200000
";
        std::fs::write(&path, original).unwrap();

        install(&path).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.starts_with(original), "{written}");
        let hooks = parsed(&path)["hooks"].as_array().unwrap().clone();
        assert_eq!(hooks.len(), crate::agents::KIMI_SIDECAR_EVENTS.len());
        assert!(hooks
            .iter()
            .all(|h| is_aoe_hook_command(h["command"].as_str().unwrap())));

        assert!(uninstall_kimi_hooks(&path).unwrap());
        assert!(parsed(&path).get("hooks").is_none());
        assert_eq!(parsed(&path)["default_model"].as_str(), Some("kimi-k2"));
        assert!(!uninstall_kimi_hooks(&path).unwrap());
    }

    #[test]
    fn kimi_hooks_migrate_inline_array() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "hooks = [{ event = \"SessionStart\", command = \"echo hi\" }]\n",
        )
        .unwrap();

        install(&path).unwrap();
        let hooks = parsed(&path)["hooks"].as_array().unwrap().clone();
        assert_eq!(hooks.len(), crate::agents::KIMI_SIDECAR_EVENTS.len() + 1);
        assert_eq!(hooks[0]["command"].as_str(), Some("echo hi"));
        assert_eq!(hooks[0]["event"].as_str(), Some("SessionStart"));

        assert!(uninstall_kimi_hooks(&path).unwrap());
        let hooks = parsed(&path)["hooks"].as_array().unwrap().clone();
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0]["command"].as_str(), Some("echo hi"));

        let original =
            "hooks = [{ event = \"SessionStart\", command = \"echo hi\" }, \"custom\"]\n";
        std::fs::write(&path, original).unwrap();
        assert!(install(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }
}
