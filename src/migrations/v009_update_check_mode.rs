//! Migration v009: Replace `[updates] check_enabled` with `update_check_mode`.
//!
//! Schema change for #1140. The legacy boolean only covered on/off; the new
//! enum adds a third mode (`auto`) that quietly installs releases in the
//! background. Without this migration, users who had `check_enabled = false`
//! would silently flip back to the default `notify` mode on upgrade because
//! serde drops unknown fields. The mapping:
//!
//! - `check_enabled = false` => `update_check_mode = "off"`
//! - `check_enabled = true`  => `update_check_mode = "notify"` (the default)
//! - field missing           => no-op (`update_check_mode` already defaults
//!   to `notify` via serde)
//!
//! Also drops the orphaned `auto_update` boolean that lingered in older
//! configs (it was never wired to anything; see the legacy-fields test in
//! `src/session/config.rs`).

use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;
use tracing::{debug, info};

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;

    let global_config = app_dir.join("config.toml");
    migrate_config_file(&global_config)?;

    let profiles_dir = app_dir.join("profiles");
    if profiles_dir.exists() {
        for entry in fs::read_dir(&profiles_dir)? {
            let entry = entry?;
            if entry.path().is_dir() {
                let profile_config = entry.path().join("config.toml");
                migrate_config_file(&profile_config)?;
            }
        }
    }

    Ok(())
}

fn migrate_config_file(path: &PathBuf) -> Result<()> {
    if !path.exists() {
        debug!("Config file {} does not exist, skipping", path.display());
        return Ok(());
    }

    let content = fs::read_to_string(path)?;
    // Unlike older migrations that swallow parse errors, this one maps a
    // user-set opt-out (`check_enabled = false`) into the new enum. If we
    // marked the migration done on a parse failure here, a user who fixed
    // their TOML afterwards would silently lose the opt-out (serde drops
    // unknown fields, so `check_enabled` would never be re-read). Bail
    // instead and let the user see the error.
    let mut doc: toml::Table = content
        .parse()
        .with_context(|| format!("Failed to parse {} during v009 migration", path.display()))?;

    let Some(updates) = doc.get_mut("updates").and_then(|u| u.as_table_mut()) else {
        debug!("No [updates] section in {}, skipping", path.display());
        return Ok(());
    };

    // Pull the legacy fields out. `auto_update` was never wired up so it
    // gets dropped unconditionally; `check_enabled` determines the new
    // `update_check_mode` value when present.
    let legacy_check_enabled = updates.remove("check_enabled");
    let _ = updates.remove("auto_update");

    // If the user already has `update_check_mode` set (manual edit or
    // future re-run of this migration), don't clobber it.
    if updates.contains_key("update_check_mode") {
        debug!(
            "{} already has update_check_mode, dropping legacy fields only",
            path.display()
        );
    } else if let Some(value) = legacy_check_enabled {
        let mode = match value.as_bool() {
            Some(false) => "off",
            Some(true) => "notify",
            None => "notify",
        };
        info!(
            "Migrating updates.check_enabled -> update_check_mode = \"{}\" in {}",
            mode,
            path.display()
        );
        updates.insert(
            "update_check_mode".to_string(),
            toml::Value::String(mode.to_string()),
        );
    }

    let new_content = toml::to_string_pretty(&doc)?;
    crate::session::atomic_write(path, new_content.as_bytes())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(content: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, content).unwrap();
        (dir, path)
    }

    #[test]
    fn test_check_enabled_false_maps_to_off() {
        let (_dir, path) = write(
            r#"
[updates]
check_enabled = false
check_interval_hours = 12
"#,
        );
        migrate_config_file(&path).unwrap();
        let result: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(result["updates"]["update_check_mode"].as_str(), Some("off"));
        assert!(result["updates"]
            .as_table()
            .unwrap()
            .get("check_enabled")
            .is_none());
        assert_eq!(
            result["updates"]["check_interval_hours"].as_integer(),
            Some(12)
        );
    }

    #[test]
    fn test_check_enabled_true_maps_to_notify() {
        let (_dir, path) = write(
            r#"
[updates]
check_enabled = true
"#,
        );
        migrate_config_file(&path).unwrap();
        let result: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(
            result["updates"]["update_check_mode"].as_str(),
            Some("notify")
        );
    }

    #[test]
    fn test_auto_update_field_is_dropped() {
        let (_dir, path) = write(
            r#"
[updates]
check_enabled = true
auto_update = true
"#,
        );
        migrate_config_file(&path).unwrap();
        let result: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert!(result["updates"]
            .as_table()
            .unwrap()
            .get("auto_update")
            .is_none());
    }

    #[test]
    fn test_already_migrated_is_idempotent() {
        let (_dir, path) = write(
            r#"
[updates]
update_check_mode = "auto"
"#,
        );
        migrate_config_file(&path).unwrap();
        let result: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(
            result["updates"]["update_check_mode"].as_str(),
            Some("auto")
        );
    }

    #[test]
    fn test_existing_mode_wins_over_legacy_check_enabled() {
        let (_dir, path) = write(
            r#"
[updates]
update_check_mode = "auto"
check_enabled = false
"#,
        );
        migrate_config_file(&path).unwrap();
        let result: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(
            result["updates"]["update_check_mode"].as_str(),
            Some("auto")
        );
        assert!(result["updates"]
            .as_table()
            .unwrap()
            .get("check_enabled")
            .is_none());
    }

    #[test]
    fn test_no_updates_section_is_noop() {
        let (_dir, path) = write(
            r#"
[session]
default_tool = "claude"
"#,
        );
        migrate_config_file(&path).unwrap();
        let result: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(result["session"]["default_tool"].as_str(), Some("claude"));
        assert!(result.get("updates").is_none());
    }

    #[test]
    fn test_nonexistent_file_is_noop() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.toml");
        migrate_config_file(&path).unwrap();
    }
}
