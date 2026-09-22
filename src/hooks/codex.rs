//! Codex hooks: `hooks.json` installs gated on `config.toml`, and the legacy
//! `config.toml` hook tables that migrations still rewrite.

use std::path::Path;

use anyhow::{Context, Result};
use toml_edit::{DocumentMut, Item, TableLike};

use crate::agents::ResolvedHookEvent;

use super::command::{hook_command, is_aoe_hook_command};
use super::config_io::{
    log_installed, log_removed, log_unchanged, with_config_lock_policy, SymlinkPolicy,
};
use super::HookInstallTarget;

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

/// Install Codex JSON hooks unless the adjacent `config.toml` disables them.
/// Empty events remove AoE hooks regardless. Sandbox config must be absent or
/// safely readable without following links; an unreadable config aborts.
pub(crate) fn install_codex_json_hooks(
    hooks_path: &Path,
    events: impl AsRef<[ResolvedHookEvent]>,
    target: HookInstallTarget,
) -> Result<()> {
    let events = events.as_ref();
    if events.is_empty() {
        return super::install_hooks(hooks_path, events, target);
    }

    let config_path = hooks_path.with_file_name("config.toml");
    let config = match target {
        HookInstallTarget::Host => read_codex_config(&config_path, SymlinkPolicy::Follow)?,
        HookInstallTarget::Sandbox => match std::fs::symlink_metadata(&config_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => DocumentMut::new(),
            Err(error) => return Err(error).context("Inspecting sandbox Codex config"),
            Ok(_) => {
                // `Never` maps unsafe entries to absence; that must not read as opted in.
                let content = SymlinkPolicy::Never.read(&config_path)?.with_context(|| {
                    format!(
                        "Cannot safely read sandbox Codex config {}",
                        config_path.display()
                    )
                })?;
                content
                    .parse::<DocumentMut>()
                    .with_context(|| format!("Failed to parse {}", config_path.display()))?
            }
        },
    };
    if codex_hooks_feature_is_disabled(&config, &config_path) {
        return Ok(());
    }
    super::install_hooks(hooks_path, events, target)
}

/// Read `[hooks.state]` (Codex's hook trust records) under the config lock.
pub(crate) fn snapshot_codex_hooks_state(config_path: &Path) -> Result<Option<Item>> {
    if !config_path.exists() {
        return Ok(None);
    }
    with_codex_config_lock(config_path, SymlinkPolicy::Follow, || {
        let config = read_codex_config(config_path, SymlinkPolicy::Follow)?;
        Ok(config
            .get("hooks")
            .and_then(Item::as_table_like)
            .and_then(|hooks| hooks.get("state"))
            .cloned())
    })
}

/// Overwrite `[hooks.state]` with a snapshot taken by [`snapshot_codex_hooks_state`].
pub(crate) fn restore_codex_hooks_state(config_path: &Path, state: Item) -> Result<()> {
    with_codex_config_lock(config_path, SymlinkPolicy::Follow, || {
        let mut config = read_codex_config(config_path, SymlinkPolicy::Follow)?;
        ensure_codex_hooks_table(&mut config)?.insert("state", state);
        write_codex_config(config_path, &config, SymlinkPolicy::Follow)
    })
}

/// Rewrite AoE's `config.toml` hook tables, seeding `[hooks.state]` from
/// `preserved_state` only when the file has none. Unchanged content is not rewritten.
pub(crate) fn install_codex_hooks_with_preserved_state(
    config_path: &Path,
    events: impl AsRef<[ResolvedHookEvent]>,
    preserved_state: Option<Item>,
    target: HookInstallTarget,
) -> Result<()> {
    with_codex_config_lock(config_path, SymlinkPolicy::Follow, || {
        let mut config = read_codex_config(config_path, SymlinkPolicy::Follow)?;
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
        let hooks = ensure_codex_hooks_table(&mut config)?;
        for event in events.as_ref() {
            if let Some(status) = event.status {
                ensure_codex_event_array(hooks, &event.name)?.push(codex_matcher_group(
                    event,
                    &hook_command(status.as_str(), target),
                ));
            }
        }

        if config.to_string() == before {
            log_unchanged(config_path);
            return Ok(());
        }
        write_codex_config(config_path, &config, SymlinkPolicy::Follow)?;
        log_installed(config_path);
        Ok(())
    })
}

/// Remove AoE status hooks from Codex's `config.toml`.
pub fn uninstall_codex_hooks(config_path: &Path) -> Result<bool> {
    if !config_path.exists() {
        return Ok(false);
    }
    let modified = with_codex_config_lock(config_path, SymlinkPolicy::Follow, || {
        let mut config = read_codex_config(config_path, SymlinkPolicy::Follow)?;
        if !remove_codex_aoe_hooks(&mut config)? {
            return Ok(false);
        }
        write_codex_config(config_path, &config, SymlinkPolicy::Follow)?;
        Ok(true)
    })?;
    if modified {
        log_removed(config_path);
    }
    Ok(modified)
}

pub(super) fn with_codex_config_lock<T>(
    config_path: &Path,
    policy: SymlinkPolicy,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
    with_config_lock_policy(&policy.lock_path(config_path)?, "toml.lock", policy, f)
}

pub(super) fn write_codex_config(
    config_path: &Path,
    config: &DocumentMut,
    policy: SymlinkPolicy,
) -> Result<()> {
    policy.write(config_path, config.to_string().as_bytes())
}

pub(super) fn read_codex_config(config_path: &Path, policy: SymlinkPolicy) -> Result<DocumentMut> {
    match policy.read(config_path)? {
        Some(content) => content
            .parse::<DocumentMut>()
            .with_context(|| format!("Failed to parse {}", config_path.display())),
        None => Ok(DocumentMut::new()),
    }
}

fn ensure_codex_hooks_table(config: &mut DocumentMut) -> Result<&mut toml_edit::Table> {
    let not_table = || anyhow::anyhow!("Codex hooks key is not a TOML table");
    let hooks = config
        .as_table_mut()
        .entry("hooks")
        .or_insert_with(|| Item::Table(toml_edit::Table::new()));
    if !hooks.is_table() {
        match std::mem::take(hooks).into_table() {
            Ok(table) => *hooks = Item::Table(table),
            Err(old) => {
                *hooks = old;
                return Err(not_table());
            }
        }
    }
    hooks.as_table_mut().ok_or_else(not_table)
}

fn ensure_codex_event_array<'a>(
    hooks: &'a mut toml_edit::Table,
    event_name: &str,
) -> Result<&'a mut toml_edit::ArrayOfTables> {
    let not_array =
        || anyhow::anyhow!("Codex hooks.{event_name} is not an array of matcher groups");
    let item = hooks
        .entry(event_name)
        .or_insert_with(|| Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
    if !item.is_array_of_tables() {
        if item.as_array().is_some_and(|arr| arr.is_empty()) {
            *item = Item::ArrayOfTables(toml_edit::ArrayOfTables::new());
        } else {
            match std::mem::take(item).into_array_of_tables() {
                Ok(array) => *item = Item::ArrayOfTables(array),
                Err(old) => {
                    *item = old;
                    return Err(not_array());
                }
            }
        }
    }
    item.as_array_of_tables_mut().ok_or_else(not_array)
}

fn codex_matcher_group(event: &ResolvedHookEvent, command: &str) -> toml_edit::Table {
    let mut group = toml_edit::Table::new();
    if let Some(matcher) = &event.matcher {
        group.insert("matcher", toml_edit::value(matcher.as_str()));
    }
    let mut handler = toml_edit::Table::new();
    handler.insert("type", toml_edit::value("command"));
    handler.insert("command", toml_edit::value(command));
    let mut handlers = toml_edit::ArrayOfTables::new();
    handlers.push(handler);
    group.insert("hooks", Item::ArrayOfTables(handlers));
    group
}

/// For each handler in a matcher group's `hooks` (table or inline form),
/// whether its command is AoE's.
pub(super) fn codex_group_aoe_flags(group: &dyn TableLike) -> Vec<bool> {
    let Some(hooks) = group.get("hooks") else {
        return Vec::new();
    };
    if let Some(handlers) = hooks.as_array_of_tables() {
        return handlers
            .iter()
            .map(|h| {
                h.get("command")
                    .and_then(Item::as_str)
                    .is_some_and(is_aoe_hook_command)
            })
            .collect();
    }
    hooks
        .as_array()
        .into_iter()
        .flatten()
        .map(|h| {
            h.as_inline_table()
                .and_then(|h| h.get("command"))
                .and_then(toml_edit::Value::as_str)
                .is_some_and(is_aoe_hook_command)
        })
        .collect()
}

fn codex_group_is_all_aoe(group: &dyn TableLike) -> bool {
    let flags = codex_group_aoe_flags(group);
    !flags.is_empty() && flags.iter().all(|aoe| *aoe)
}

fn remove_codex_aoe_hooks(config: &mut DocumentMut) -> Result<bool> {
    let Some(hooks_item) = config.as_table_mut().get_mut("hooks") else {
        return Ok(false);
    };
    let hooks = hooks_item
        .as_table_like_mut()
        .ok_or_else(|| anyhow::anyhow!("Codex hooks key is not a TOML table"))?;

    let mut modified = false;
    for event_name in CODEX_HOOK_EVENT_NAMES {
        let Some(event_item) = hooks.get_mut(event_name) else {
            continue;
        };
        let remaining = if let Some(groups) = event_item.as_array_of_tables_mut() {
            let before = groups.len();
            groups.retain(|group| !codex_group_is_all_aoe(group));
            modified |= groups.len() != before;
            groups.len()
        } else if let Some(groups) = event_item.as_array_mut() {
            let before = groups.len();
            groups.retain(|group| {
                !group
                    .as_inline_table()
                    .is_some_and(|g| codex_group_is_all_aoe(g))
            });
            modified |= groups.len() != before;
            groups.len()
        } else {
            continue;
        };
        if remaining == 0 {
            hooks.remove(event_name);
        }
    }

    if hooks.is_empty() {
        config.as_table_mut().remove("hooks");
        modified = true;
    }
    Ok(modified)
}

fn codex_hooks_feature_is_disabled(config: &DocumentMut, config_path: &Path) -> bool {
    let disabled = config
        .get("features")
        .and_then(Item::as_table_like)
        .and_then(|features| {
            features
                .get("hooks")
                .or_else(|| features.get("codex_hooks"))
        })
        .and_then(Item::as_bool)
        .is_some_and(|enabled| !enabled);
    if disabled {
        tracing::warn!(target: "hooks.install",
            "Codex hooks are explicitly disabled in {}; skipping AoE status hooks",
            config_path.display()
        );
    }
    disabled
}

#[cfg(test)]
pub(super) fn install_codex_hooks(
    config_path: &Path,
    events: impl AsRef<[ResolvedHookEvent]>,
) -> Result<()> {
    install_codex_hooks_with_preserved_state(config_path, events, None, HookInstallTarget::Host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::agent_events;
    use tempfile::TempDir;

    fn codex_events() -> Vec<ResolvedHookEvent> {
        agent_events("codex", &[])
    }

    fn read_toml(path: &Path) -> (String, toml::Value) {
        let text = std::fs::read_to_string(path).unwrap();
        let value = toml::from_str(&text).unwrap();
        (text, value)
    }

    #[cfg(unix)]
    #[test]
    fn codex_json_config_links_are_host_only() {
        let tmp = TempDir::new().unwrap();
        let config = tmp.path().join("config.toml");
        let hooks = tmp.path().join("hooks.json");
        let linked = tmp.path().join("linked.toml");
        std::fs::write(&linked, "[features]\nhooks = true\n").unwrap();
        std::os::unix::fs::symlink(&linked, &config).unwrap();
        let sandbox =
            || install_codex_json_hooks(&hooks, codex_events(), HookInstallTarget::Sandbox);

        assert!(sandbox().is_err());
        assert!(!hooks.exists());
        install_codex_json_hooks(&hooks, codex_events(), HookInstallTarget::Host).unwrap();
        let installed = std::fs::read_to_string(&hooks).unwrap();
        assert!(installed.contains("aoe-hooks"));
        assert!(std::fs::symlink_metadata(&config)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(sandbox().is_err());
        assert_eq!(std::fs::read_to_string(&hooks).unwrap(), installed);

        std::fs::remove_file(&linked).unwrap();
        assert!(sandbox().is_err(), "a dangling link is still unsafe");
        assert_eq!(std::fs::read_to_string(&hooks).unwrap(), installed);
        std::fs::remove_file(&config).unwrap();
        std::fs::remove_file(&hooks).unwrap();
        sandbox().unwrap();
        assert!(std::fs::read_to_string(&hooks)
            .unwrap()
            .contains("aoe-hooks"));
    }

    #[test]
    fn codex_toml_install_merges_with_user_config() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            r#"# user comment
model = "gpt-5.3-codex"
approval_policy = "on-failure"
hooks = { PreToolUse = [{ matcher = "Bash", hooks = [{ type = "command", command = "echo user-hook" }] }], state = { user = { enabled = true, trusted_hash = "keep" } } }
"#,
        )
        .unwrap();

        install_codex_hooks(&path, codex_events()).unwrap();
        install_codex_hooks(&path, codex_events()).unwrap();

        let (text, config) = read_toml(&path);
        assert!(text.contains("# user comment"), "{text}");
        assert_eq!(config["approval_policy"].as_str(), Some("on-failure"));
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
        assert_eq!(text.matches("sh -c").count(), codex_events().len());

        assert!(uninstall_codex_hooks(&path).unwrap());
        let (text, _) = read_toml(&path);
        assert!(text.contains("echo user-hook"));
        assert!(text.contains("trusted_hash = \"keep\""));
        assert!(!text.contains("aoe-hooks"));
    }

    #[test]
    fn codex_toml_install_collapses_duplicated_aoe_blocks() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let block = |event: &str, status: &str| {
            format!(
                "[[hooks.{event}]]\n\n[[hooks.{event}.hooks]]\ntype = \"command\"\ncommand = {:?}\n\n",
                hook_command(status, HookInstallTarget::Host)
            )
        };
        let once = [
            ("SessionStart", "idle"),
            ("PreToolUse", "running"),
            ("Stop", "idle"),
        ]
        .map(|(event, status)| block(event, status))
        .concat();
        std::fs::write(
            &path,
            format!(
                "[hooks]\n\n{once}{once}[hooks.state.trusted]\nenabled = true\ntrusted_hash = \"sha256:keep\"\n\n[projects.\"/tmp/aoe-project\"]\ntrust_level = \"trusted\"\n"
            ),
        )
        .unwrap();

        install_codex_hooks(&path, codex_events()).unwrap();

        let (text, config) = read_toml(&path);
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
        assert_eq!(text.matches("sh -c").count(), codex_events().len());
    }

    #[test]
    fn codex_toml_install_keeps_newer_hooks_state() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(
            &path,
            "[hooks.state.current]\nenabled = true\ntrusted_hash = \"new\"\n",
        )
        .unwrap();
        let mut stale = toml_edit::Table::new();
        stale.insert("trusted_hash", toml_edit::value("old"));
        let mut preserved = toml_edit::Table::new();
        preserved.insert("stale", Item::Table(stale));

        install_codex_hooks_with_preserved_state(
            &path,
            codex_events(),
            Some(Item::Table(preserved)),
            HookInstallTarget::Host,
        )
        .unwrap();

        let (_, config) = read_toml(&path);
        assert_eq!(
            config["hooks"]["state"]["current"]["trusted_hash"].as_str(),
            Some("new")
        );
        assert!(config["hooks"]["state"].get("stale").is_none());
    }

    #[test]
    fn codex_toml_install_respects_disabled_feature() {
        for original in [
            "# keep this comment\nmodel = \"gpt-5.3-codex\"\n\n[features]\nweb_search = true\nhooks = false\n",
            "model = \"gpt-5.3-codex\"\nfeatures = { web_search = true, hooks = false }\n",
            "model = \"gpt-5.3-codex\"\nfeatures = { web_search = true, codex_hooks = false }\n",
        ] {
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join("config.toml");
            std::fs::write(&path, original).unwrap();
            install_codex_hooks(&path, codex_events()).unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        }
    }

    #[test]
    fn codex_toml_concurrent_rewrites_keep_valid_toml() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "model = \"gpt-5.3-codex\"\n\n[projects.\"/tmp/aoe-project\"]\ntrust_level = \"trusted\"\n").unwrap();

        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    barrier.wait();
                    for _ in 0..8 {
                        install_codex_hooks(&path, codex_events()).unwrap();
                        read_toml(&path);
                    }
                });
            }
        });

        let (text, config) = read_toml(&path);
        for event in codex_events() {
            assert_eq!(config["hooks"][event.name].as_array().unwrap().len(), 1);
        }
        assert_eq!(
            config["projects"]["/tmp/aoe-project"]["trust_level"].as_str(),
            Some("trusted")
        );
        assert_eq!(text.matches("sh -c").count(), codex_events().len());
    }

    #[cfg(unix)]
    #[test]
    fn codex_toml_install_preserves_symlinked_config() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".codex")).unwrap();
        std::fs::create_dir_all(tmp.path().join("dotfiles")).unwrap();
        let target = tmp.path().join("dotfiles/codex-config.toml");
        std::fs::write(&target, "model = \"gpt-5.3-codex\"\n").unwrap();
        let link = tmp.path().join(".codex/config.toml");
        std::os::unix::fs::symlink("../dotfiles/codex-config.toml", &link).unwrap();
        let is_link = || {
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        };

        install_codex_hooks(&link, codex_events()).unwrap();
        assert!(is_link());
        let (text, config) = read_toml(&target);
        assert!(text.contains("model = \"gpt-5.3-codex\""));
        assert!(config["hooks"]["SessionStart"].is_array());
        assert!(target.with_extension("toml.lock").exists());
        assert!(!link.with_extension("toml.lock").exists());

        uninstall_codex_hooks(&link).unwrap();
        assert!(is_link());
        let (text, config) = read_toml(&target);
        assert!(text.contains("model = \"gpt-5.3-codex\""));
        assert!(config.get("hooks").is_none());
    }
}
