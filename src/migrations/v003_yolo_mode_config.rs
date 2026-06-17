//! Migration v003: Move yolo_mode_default from [sandbox] to [session]
//!
//! Previously: [sandbox] yolo_mode_default = true
//! After:      [session] yolo_mode_default = true
//!
//! This migration reads all config files (global + profiles) and moves
//! the yolo_mode_default setting from the sandbox section to the session section.
//! Without this, users who had yolo_mode_default enabled would silently lose the setting.

use anyhow::Result;
use std::fs;
use std::path::PathBuf;
use tracing::{debug, info};

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;

    // Migrate global config
    let global_config = app_dir.join("config.toml");
    migrate_config_file(&global_config)?;

    // Migrate all profile configs
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
    let mut doc: toml::Table = match content.parse() {
        Ok(table) => table,
        Err(e) => {
            debug!("Failed to parse {}: {}, skipping", path.display(), e);
            return Ok(());
        }
    };

    // Check if [sandbox] has yolo_mode_default
    let yolo_value = doc
        .get("sandbox")
        .and_then(|s| s.as_table())
        .and_then(|t| t.get("yolo_mode_default"))
        .and_then(|v| v.as_bool());

    let Some(yolo_enabled) = yolo_value else {
        debug!(
            "No [sandbox] yolo_mode_default in {}, skipping",
            path.display()
        );
        return Ok(());
    };

    info!(
        "Migrating yolo_mode_default={} from [sandbox] to [session] in {}",
        yolo_enabled,
        path.display()
    );

    // Remove from [sandbox]
    if let Some(sandbox) = doc.get_mut("sandbox").and_then(|s| s.as_table_mut()) {
        sandbox.remove("yolo_mode_default");
    }

    // Set in [session] (only if it was true -- false is the default anyway)
    if yolo_enabled {
        let session = doc
            .entry("session")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .expect("session should be a table");

        // Only set if not already present (don't overwrite an explicit value)
        session
            .entry("yolo_mode_default")
            .or_insert(toml::Value::Boolean(true));
    }

    let new_content = toml::to_string_pretty(&doc)?;
    crate::session::atomic_write(path, new_content.as_bytes())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_migrate_yolo_from_sandbox_to_session() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        let content = r#"
[sandbox]
enabled_by_default = false
yolo_mode_default = true
default_image = "ghcr.io/njbrake/aoe-sandbox:latest"

[session]
default_tool = "claude"
"#;
        fs::write(&config_path, content).unwrap();

        migrate_config_file(&config_path.to_path_buf()).unwrap();

        let result: toml::Table = fs::read_to_string(&config_path).unwrap().parse().unwrap();

        // yolo_mode_default should be under [session]
        assert_eq!(result["session"]["yolo_mode_default"].as_bool(), Some(true));

        // yolo_mode_default should be removed from [sandbox]
        assert!(result["sandbox"]
            .as_table()
            .unwrap()
            .get("yolo_mode_default")
            .is_none());

        // Other sandbox settings should be preserved
        assert_eq!(
            result["sandbox"]["enabled_by_default"].as_bool(),
            Some(false)
        );
    }

    #[test]
    fn test_migrate_yolo_false_not_set_in_session() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        let content = r#"
[sandbox]
yolo_mode_default = false
"#;
        fs::write(&config_path, content).unwrap();

        migrate_config_file(&config_path.to_path_buf()).unwrap();

        let result: toml::Table = fs::read_to_string(&config_path).unwrap().parse().unwrap();

        // yolo_mode_default=false is the default, so no need to add to [session]
        assert!(result.get("session").is_none());

        // Should still be removed from [sandbox]
        assert!(result["sandbox"]
            .as_table()
            .unwrap()
            .get("yolo_mode_default")
            .is_none());
    }

    #[test]
    fn test_migrate_no_sandbox_section() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        let content = r#"
[session]
default_tool = "claude"
"#;
        fs::write(&config_path, content).unwrap();

        migrate_config_file(&config_path.to_path_buf()).unwrap();

        let result: toml::Table = fs::read_to_string(&config_path).unwrap().parse().unwrap();

        // Nothing should change
        assert_eq!(result["session"]["default_tool"].as_str(), Some("claude"));
        assert!(result["session"]
            .as_table()
            .unwrap()
            .get("yolo_mode_default")
            .is_none());
    }

    #[test]
    fn test_migrate_nonexistent_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("nonexistent.toml");

        // Should not error
        migrate_config_file(&config_path.to_path_buf()).unwrap();
    }

    #[test]
    fn test_migrate_does_not_overwrite_existing_session_value() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        let content = r#"
[sandbox]
yolo_mode_default = true

[session]
yolo_mode_default = false
"#;
        fs::write(&config_path, content).unwrap();

        migrate_config_file(&config_path.to_path_buf()).unwrap();

        let result: toml::Table = fs::read_to_string(&config_path).unwrap().parse().unwrap();

        // Should preserve the existing [session] value (false), not overwrite with sandbox's true
        assert_eq!(
            result["session"]["yolo_mode_default"].as_bool(),
            Some(false)
        );
    }
}
