//! Migration v003: Move `yolo_mode_default` from `[sandbox]` to `[session]`.
//!
//! Applies to global and profile configs, preserving the existing value.

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
    config_file::rewrite(path, |doc| {
        let Some(enabled) = doc
            .get("sandbox")
            .and_then(|s| s.as_table())
            .and_then(|t| t.get("yolo_mode_default"))
            .and_then(|v| v.as_bool())
        else {
            return false;
        };
        info!(
            "Migrating yolo_mode_default={} from [sandbox] to [session] in {}",
            enabled,
            path.display()
        );
        if let Some(sandbox) = doc.get_mut("sandbox").and_then(|s| s.as_table_mut()) {
            sandbox.remove("yolo_mode_default");
        }
        // `false` is the new default, so only a `true` needs carrying over, and
        // never over an explicit value already there.
        if enabled {
            if let Some(session) = doc
                .entry("session")
                .or_insert_with(|| toml::Value::Table(toml::Table::new()))
                .as_table_mut()
            {
                session
                    .entry("yolo_mode_default")
                    .or_insert(toml::Value::Boolean(true));
            }
        }
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
