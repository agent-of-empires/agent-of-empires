//! Migration v004: Merge environment_values into environment
//!
//! Previously: [sandbox] environment = ["TERM", ...] (pass-through keys)
//!             [sandbox] environment_values = { GH_TOKEN = "$GH_TOKEN" } (explicit KEY=VALUE)
//! After:      [sandbox] environment = ["TERM", ..., "GH_TOKEN=$GH_TOKEN"] (unified list)
//!
//! Entries in the unified list follow the convention:
//! - `KEY` (no `=`) = pass through host value
//! - `KEY=VALUE` = set explicit value; VALUE supports `$HOST_VAR` and `$$` escaping

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

    let env_values = doc
        .get("sandbox")
        .and_then(|s| s.as_table())
        .and_then(|t| t.get("environment_values"))
        .and_then(|v| v.as_table())
        .cloned();

    let Some(values_table) = env_values else {
        debug!(
            "No [sandbox] environment_values in {}, skipping",
            path.display()
        );
        return Ok(());
    };

    if values_table.is_empty() {
        // Just remove the empty table
        if let Some(sandbox) = doc.get_mut("sandbox").and_then(|s| s.as_table_mut()) {
            sandbox.remove("environment_values");
        }
        let new_content = toml::to_string_pretty(&doc)?;
        crate::session::atomic_write(path, new_content.as_bytes())?;
        return Ok(());
    }

    info!(
        "Migrating {} environment_values entries into environment list in {}",
        values_table.len(),
        path.display()
    );

    // Convert each (key, value) to "key=value" and append to environment array
    let sandbox = doc
        .get_mut("sandbox")
        .and_then(|s| s.as_table_mut())
        .expect("sandbox table should exist");

    // Get or create the environment array
    let env_array = sandbox
        .entry("environment")
        .or_insert_with(|| toml::Value::Array(Vec::new()));

    if let Some(arr) = env_array.as_array_mut() {
        for (key, val) in &values_table {
            if let Some(v) = val.as_str() {
                arr.push(toml::Value::String(format!("{}={}", key, v)));
            }
        }
    }

    // Remove the old environment_values key
    sandbox.remove("environment_values");

    let new_content = toml::to_string_pretty(&doc)?;
    crate::session::atomic_write(path, new_content.as_bytes())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_migrate_env_values_into_environment() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        let content = r#"
[sandbox]
enabled_by_default = false
environment = ["TERM", "COLORTERM"]

[sandbox.environment_values]
GH_TOKEN = "$GH_TOKEN"
MY_VAR = "literal_value"
"#;
        fs::write(&config_path, content).unwrap();

        migrate_config_file(&config_path.to_path_buf()).unwrap();

        let result: toml::Table = fs::read_to_string(&config_path).unwrap().parse().unwrap();

        // environment_values should be removed
        assert!(result["sandbox"]
            .as_table()
            .unwrap()
            .get("environment_values")
            .is_none());

        // environment should contain original entries plus migrated ones
        let env = result["sandbox"]["environment"].as_array().unwrap();
        let env_strings: Vec<&str> = env.iter().filter_map(|v| v.as_str()).collect();
        assert!(env_strings.contains(&"TERM"));
        assert!(env_strings.contains(&"COLORTERM"));
        assert!(env_strings.contains(&"GH_TOKEN=$GH_TOKEN"));
        assert!(env_strings.contains(&"MY_VAR=literal_value"));
    }

    #[test]
    fn test_migrate_no_environment_values() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        let content = r#"
[sandbox]
enabled_by_default = false
environment = ["TERM"]
"#;
        fs::write(&config_path, content).unwrap();

        migrate_config_file(&config_path.to_path_buf()).unwrap();

        let result: toml::Table = fs::read_to_string(&config_path).unwrap().parse().unwrap();
        let env = result["sandbox"]["environment"].as_array().unwrap();
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].as_str(), Some("TERM"));
    }

    #[test]
    fn test_migrate_empty_environment_values() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        let content = r#"
[sandbox]
environment = ["TERM"]

[sandbox.environment_values]
"#;
        fs::write(&config_path, content).unwrap();

        migrate_config_file(&config_path.to_path_buf()).unwrap();

        let result: toml::Table = fs::read_to_string(&config_path).unwrap().parse().unwrap();
        assert!(result["sandbox"]
            .as_table()
            .unwrap()
            .get("environment_values")
            .is_none());
        let env = result["sandbox"]["environment"].as_array().unwrap();
        assert_eq!(env.len(), 1);
    }

    #[test]
    fn test_migrate_creates_environment_array_if_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");

        let content = r#"
[sandbox]
enabled_by_default = false

[sandbox.environment_values]
TOKEN = "secret"
"#;
        fs::write(&config_path, content).unwrap();

        migrate_config_file(&config_path.to_path_buf()).unwrap();

        let result: toml::Table = fs::read_to_string(&config_path).unwrap().parse().unwrap();
        let env = result["sandbox"]["environment"].as_array().unwrap();
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].as_str(), Some("TOKEN=secret"));
    }

    #[test]
    fn test_migrate_nonexistent_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("nonexistent.toml");
        migrate_config_file(&config_path.to_path_buf()).unwrap();
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
        assert_eq!(result["session"]["default_tool"].as_str(), Some("claude"));
    }
}
