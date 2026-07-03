//! Migration v019: move `acp_defaults` from `[session]` to `[acp]`.
//!
//! Per-agent structured-view defaults were introduced under `[session]`, but
//! the setting is ACP/structured-view configuration and now lives on `AcpConfig`
//! so the web dashboard renders it under the Structured View tab (which is
//! section-routed to `[acp]`). Without this move an existing
//! `[session.acp_defaults.*]` value would be silently ignored (the new field
//! defaults empty), so the user would lose their configured defaults.
//!
//! Applies to the global config and every profile config. Idempotent: a value
//! already under `[acp]` is preferred and the stale `[session]` copy is dropped.

use anyhow::Result;
use std::fs;
use std::path::Path;
use tracing::{debug, info};

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    run_in(&app_dir)
}

pub(crate) fn run_in(app_dir: &Path) -> Result<()> {
    migrate_config_file(&app_dir.join("config.toml"))?;
    let profiles_dir = app_dir.join("profiles");
    if profiles_dir.exists() {
        for entry in fs::read_dir(&profiles_dir)? {
            let entry = entry?;
            if entry.path().is_dir() {
                migrate_config_file(&entry.path().join("config.toml"))?;
            }
        }
    }
    Ok(())
}

fn migrate_config_file(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let content = fs::read_to_string(path)?;
    let mut doc: toml::Table = match content.parse() {
        Ok(table) => table,
        Err(e) => {
            debug!("failed to parse {}: {e}, skipping", path.display());
            return Ok(());
        }
    };

    // Pull `acp_defaults` out of `[session]`; nothing to do if absent.
    let moved = doc
        .get_mut("session")
        .and_then(toml::Value::as_table_mut)
        .and_then(|session| session.remove("acp_defaults"));
    let Some(moved) = moved else {
        return Ok(());
    };

    // Insert under `[acp]`, creating the table if needed. Prefer an existing
    // `[acp].acp_defaults` (a prior run or manual edit) over the stale copy.
    let acp = doc
        .entry("acp".to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if let Some(acp_table) = acp.as_table_mut() {
        acp_table.entry("acp_defaults".to_string()).or_insert(moved);
    }

    crate::session::atomic_write(path, toml::to_string_pretty(&doc)?.as_bytes())?;
    info!(
        "v019: moved acp_defaults from [session] to [acp] in {}",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_acp_defaults_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[session]\nsmart_rename = true\n\n[session.acp_defaults.opencode]\nmodel = \"openai/gpt-5.5\"\neffort = \"high\"\n",
        )
        .unwrap();

        migrate_config_file(&path).unwrap();

        let doc: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        // Moved under [acp], removed from [session], other session keys kept.
        let acp = doc["acp"].as_table().unwrap();
        let defaults = acp["acp_defaults"].as_table().unwrap();
        assert_eq!(
            defaults["opencode"].as_table().unwrap()["model"].as_str(),
            Some("openai/gpt-5.5")
        );
        assert!(!doc["session"]
            .as_table()
            .unwrap()
            .contains_key("acp_defaults"));
        assert_eq!(
            doc["session"].as_table().unwrap()["smart_rename"].as_bool(),
            Some(true)
        );

        // Idempotent: a second run leaves the file unchanged.
        let before = fs::read_to_string(&path).unwrap();
        migrate_config_file(&path).unwrap();
        assert_eq!(before, fs::read_to_string(&path).unwrap());
    }

    #[test]
    fn no_session_acp_defaults_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[session]\nsmart_rename = true\n").unwrap();
        let before = fs::read_to_string(&path).unwrap();
        migrate_config_file(&path).unwrap();
        assert_eq!(before, fs::read_to_string(&path).unwrap());
    }

    #[test]
    fn existing_acp_copy_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[session.acp_defaults.opencode]\nmodel = \"stale\"\n\n[acp.acp_defaults.opencode]\nmodel = \"fresh\"\n",
        )
        .unwrap();

        migrate_config_file(&path).unwrap();

        let doc: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        let defaults = doc["acp"].as_table().unwrap()["acp_defaults"]
            .as_table()
            .unwrap();
        assert_eq!(
            defaults["opencode"].as_table().unwrap()["model"].as_str(),
            Some("fresh")
        );
        assert!(!doc["session"]
            .as_table()
            .unwrap()
            .contains_key("acp_defaults"));
    }
}
