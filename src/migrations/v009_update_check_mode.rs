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
    use crate::migrations::test_cases::assert_rewrites;

    #[test]
    fn maps_check_enabled_onto_update_check_mode() {
        let no_updates = "[session]\ndefault_tool = \"claude\"\n";
        let migrated = "[updates]\nupdate_check_mode = \"auto\"\n";
        assert_rewrites(
            "config.toml",
            migrate_config_file,
            &[
                (
                    Some("[updates]\ncheck_enabled = false\ncheck_interval_hours = 12\n"),
                    Some("[updates]\ncheck_interval_hours = 12\nupdate_check_mode = \"off\"\n"),
                ),
                (
                    Some("[updates]\ncheck_enabled = true\nauto_update = true\n"),
                    Some("[updates]\nupdate_check_mode = \"notify\"\n"),
                ),
                (Some(migrated), Some(migrated)),
                // An existing mode wins; the legacy flag is still dropped.
                (
                    Some("[updates]\nupdate_check_mode = \"auto\"\ncheck_enabled = false\n"),
                    Some(migrated),
                ),
                (Some(no_updates), Some(no_updates)),
                (None, None),
            ],
        );
    }
}
