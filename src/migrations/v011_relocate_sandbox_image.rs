//! Migration v011: Relocate sandbox image from ghcr.io/njbrake to ghcr.io/agent-of-empires
//!
//! When the repo moved from `njbrake/agent-of-empires` to `agent-of-empires/agent-of-empires`
//! the published container images moved with it: `ghcr.io/njbrake/aoe-sandbox` and
//! `ghcr.io/njbrake/aoe-dev-sandbox` are republished as `ghcr.io/agent-of-empires/aoe-sandbox`
//! and `ghcr.io/agent-of-empires/aoe-dev-sandbox`. GHCR keeps the old paths alive as redirects
//! for now, but they should not be the canonical reference in stored config.
//!
//! This migration rewrites `[sandbox] default_image` in the global config and every profile
//! config to point at the new namespace. Idempotent: re-running on already-migrated configs
//! is a no-op.

use anyhow::Result;
use std::fs;
use std::path::PathBuf;
use tracing::{debug, info};

const OLD_NAMESPACE: &str = "ghcr.io/njbrake/";
const NEW_NAMESPACE: &str = "ghcr.io/agent-of-empires/";

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
    let mut doc: toml::Table = match content.parse() {
        Ok(table) => table,
        Err(e) => {
            debug!("Failed to parse {}: {}, skipping", path.display(), e);
            return Ok(());
        }
    };

    let Some(sandbox) = doc.get_mut("sandbox").and_then(|s| s.as_table_mut()) else {
        return Ok(());
    };

    let Some(value) = sandbox.get("default_image").and_then(|v| v.as_str()) else {
        return Ok(());
    };

    if !value.starts_with(OLD_NAMESPACE) {
        return Ok(());
    }

    let new_value = format!("{}{}", NEW_NAMESPACE, &value[OLD_NAMESPACE.len()..]);
    info!(
        "Relocating sandbox default_image: {} -> {} in {}",
        value,
        new_value,
        path.display()
    );

    sandbox.insert("default_image".to_string(), toml::Value::String(new_value));

    let new_content = toml::to_string_pretty(&doc)?;
    crate::session::atomic_write(path, new_content.as_bytes())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rewrites_aoe_sandbox() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(
            &config_path,
            r#"[sandbox]
default_image = "ghcr.io/njbrake/aoe-sandbox:latest"
"#,
        )
        .unwrap();

        migrate_config_file(&config_path.to_path_buf()).unwrap();

        let result: toml::Table = fs::read_to_string(&config_path).unwrap().parse().unwrap();
        assert_eq!(
            result["sandbox"]["default_image"].as_str(),
            Some("ghcr.io/agent-of-empires/aoe-sandbox:latest")
        );
    }

    #[test]
    fn test_rewrites_aoe_dev_sandbox() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(
            &config_path,
            r#"[sandbox]
default_image = "ghcr.io/njbrake/aoe-dev-sandbox:0.10"
"#,
        )
        .unwrap();

        migrate_config_file(&config_path.to_path_buf()).unwrap();

        let result: toml::Table = fs::read_to_string(&config_path).unwrap().parse().unwrap();
        assert_eq!(
            result["sandbox"]["default_image"].as_str(),
            Some("ghcr.io/agent-of-empires/aoe-dev-sandbox:0.10")
        );
    }

    #[test]
    fn test_idempotent_on_already_migrated() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        let original = r#"[sandbox]
default_image = "ghcr.io/agent-of-empires/aoe-sandbox:latest"
"#;
        fs::write(&config_path, original).unwrap();

        migrate_config_file(&config_path.to_path_buf()).unwrap();

        let result: toml::Table = fs::read_to_string(&config_path).unwrap().parse().unwrap();
        assert_eq!(
            result["sandbox"]["default_image"].as_str(),
            Some("ghcr.io/agent-of-empires/aoe-sandbox:latest")
        );
    }

    #[test]
    fn test_leaves_unrelated_images_alone() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(
            &config_path,
            r#"[sandbox]
default_image = "docker.io/library/ubuntu:22.04"
"#,
        )
        .unwrap();

        migrate_config_file(&config_path.to_path_buf()).unwrap();

        let result: toml::Table = fs::read_to_string(&config_path).unwrap().parse().unwrap();
        assert_eq!(
            result["sandbox"]["default_image"].as_str(),
            Some("docker.io/library/ubuntu:22.04")
        );
    }

    #[test]
    fn test_no_sandbox_section() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(
            &config_path,
            r#"[session]
default_tool = "claude"
"#,
        )
        .unwrap();

        migrate_config_file(&config_path.to_path_buf()).unwrap();

        let result: toml::Table = fs::read_to_string(&config_path).unwrap().parse().unwrap();
        assert_eq!(result["session"]["default_tool"].as_str(), Some("claude"));
    }

    #[test]
    fn test_no_default_image_set() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(
            &config_path,
            r#"[sandbox]
enabled_by_default = true
"#,
        )
        .unwrap();

        migrate_config_file(&config_path.to_path_buf()).unwrap();

        let result: toml::Table = fs::read_to_string(&config_path).unwrap().parse().unwrap();
        assert_eq!(
            result["sandbox"]["enabled_by_default"].as_bool(),
            Some(true)
        );
    }

    #[test]
    fn test_nonexistent_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("nonexistent.toml");
        migrate_config_file(&config_path.to_path_buf()).unwrap();
    }
}
