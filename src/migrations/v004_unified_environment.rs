//! Migration v004: Merge `environment_values` into `environment`.
//!
//! In `[sandbox]`, combine pass-through `environment = ["TERM", ...]` keys
//! and explicit `environment_values = { GH_TOKEN = "$GH_TOKEN" }` entries
//! into `environment = ["TERM", ..., "GH_TOKEN=$GH_TOKEN"]`.
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
    use crate::migrations::test_cases::assert_rewrites;

    #[test]
    fn folds_environment_values_into_environment() {
        let term_only = "[sandbox]\nenabled_by_default = false\nenvironment = [\"TERM\"]\n";
        let no_sandbox = "[session]\ndefault_tool = \"claude\"\n";
        assert_rewrites(
            "config.toml",
            |path| migrate_config_file(&path.to_path_buf()),
            &[
                (
                    Some(
                        "[sandbox]\nenabled_by_default = false\nenvironment = [\"TERM\", \"COLORTERM\"]\n\n\
                         [sandbox.environment_values]\nGH_TOKEN = \"$GH_TOKEN\"\nMY_VAR = \"literal_value\"\n",
                    ),
                    Some(
                        "[sandbox]\nenabled_by_default = false\nenvironment = \
                         [\"TERM\", \"COLORTERM\", \"GH_TOKEN=$GH_TOKEN\", \"MY_VAR=literal_value\"]\n",
                    ),
                ),
                (
                    Some("[sandbox]\nenvironment = [\"TERM\"]\n\n[sandbox.environment_values]\n"),
                    Some("[sandbox]\nenvironment = [\"TERM\"]\n"),
                ),
                (
                    Some("[sandbox]\nenabled_by_default = false\n\n[sandbox.environment_values]\nTOKEN = \"secret\"\n"),
                    Some("[sandbox]\nenabled_by_default = false\nenvironment = [\"TOKEN=secret\"]\n"),
                ),
                (Some(term_only), Some(term_only)),
                (Some(no_sandbox), Some(no_sandbox)),
                (None, None),
            ],
        );
    }
}
