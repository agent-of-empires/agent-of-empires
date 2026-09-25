//! Migration v009: replace `[updates] check_enabled` with `update_check_mode`
//! (`false` -> `"off"`, `true` -> `"notify"`, missing -> no-op, since serde
//! already defaults to `notify`). Also drops the orphaned `auto_update`
//! boolean older configs carry.

use super::config_file;
use anyhow::Result;
use std::path::Path;
use tracing::info;

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    for path in config_file::all_configs(&app_dir)? {
        migrate_config_file(&path)?;
    }
    Ok(())
}

fn migrate_config_file(path: &Path) -> Result<()> {
    // Strict: skipping an unparseable config would bump the schema and lose a
    // user's opt-out for good, since serde never reads `check_enabled` again.
    config_file::rewrite_strict(path, "v009", |doc| {
        let Some(updates) = doc.get_mut("updates").and_then(|u| u.as_table_mut()) else {
            return false;
        };
        // `auto_update` was never wired to anything, so it just goes.
        let legacy = updates.remove("check_enabled");
        updates.remove("auto_update");

        // An existing `update_check_mode` (manual edit, or an earlier run) wins.
        if let (false, Some(value)) = (updates.contains_key("update_check_mode"), legacy) {
            let mode = if value.as_bool() == Some(false) {
                "off"
            } else {
                "notify"
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
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

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
