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

use super::config_file;
use anyhow::Result;
use std::path::Path;
use tracing::info;

const OLD_NAMESPACE: &str = "ghcr.io/njbrake/";
const NEW_NAMESPACE: &str = "ghcr.io/agent-of-empires/";

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    for path in config_file::all_configs(&app_dir)? {
        migrate_config_file(&path)?;
    }
    Ok(())
}

fn migrate_config_file(path: &Path) -> Result<()> {
    config_file::rewrite(path, |doc| {
        let Some(sandbox) = doc.get_mut("sandbox").and_then(|s| s.as_table_mut()) else {
            return false;
        };
        let Some(value) = sandbox.get("default_image").and_then(|v| v.as_str()) else {
            return false;
        };
        let Some(rest) = value.strip_prefix(OLD_NAMESPACE) else {
            return false;
        };
        let new_value = format!("{NEW_NAMESPACE}{rest}");
        info!(
            "Relocating sandbox default_image: {} -> {} in {}",
            value,
            new_value,
            path.display()
        );
        sandbox.insert("default_image".to_string(), toml::Value::String(new_value));
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
