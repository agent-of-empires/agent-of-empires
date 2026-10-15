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
    use crate::migrations::test_cases::assert_rewrites;

    #[test]
    fn moves_a_true_yolo_default_from_sandbox_to_session() {
        let no_sandbox = "[session]\ndefault_tool = \"claude\"\n";
        assert_rewrites(
            "config.toml",
            migrate_config_file,
            &[
                (
                    Some(
                        "[sandbox]\nenabled_by_default = false\nyolo_mode_default = true\n\
                         default_image = \"ghcr.io/njbrake/aoe-sandbox:latest\"\n\n\
                         [session]\ndefault_tool = \"claude\"\n",
                    ),
                    Some(
                        "[sandbox]\nenabled_by_default = false\n\
                         default_image = \"ghcr.io/njbrake/aoe-sandbox:latest\"\n\n\
                         [session]\ndefault_tool = \"claude\"\nyolo_mode_default = true\n",
                    ),
                ),
                // false is the default, so it is dropped rather than moved.
                (
                    Some("[sandbox]\nyolo_mode_default = false\n"),
                    Some("[sandbox]\n"),
                ),
                // An existing [session] value wins over the sandbox one.
                (
                    Some("[sandbox]\nyolo_mode_default = true\n\n[session]\nyolo_mode_default = false\n"),
                    Some("[sandbox]\n\n[session]\nyolo_mode_default = false\n"),
                ),
                (Some(no_sandbox), Some(no_sandbox)),
                (None, None),
            ],
        );
    }
}
