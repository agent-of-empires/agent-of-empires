//! Migration v014: rename the `default` builtin theme to `zinc`.
//!
//! The neutral zinc + amber builtin was named `default`, which was ambiguous
//! (it read as "no theme chosen" rather than a specific look). It is now named
//! `zinc`. Theme is a single global preference (see v013), so this only has to
//! rewrite the global `config.toml`: if `[theme].name` is the literal
//! `"default"`, set it to `"zinc"`. Users on the empty default are unaffected
//! (empty still resolves to the fallback, which is now `zinc`). Idempotent: a
//! config that doesn't pin `default` is left untouched.

use super::config_file;
use anyhow::Result;
use std::path::Path;
use tracing::info;

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    rename_theme(&app_dir.join("config.toml"))
}

fn rename_theme(path: &Path) -> Result<()> {
    config_file::rewrite_strict(path, "v014", |doc| {
        let Some(theme) = doc.get_mut("theme").and_then(|t| t.as_table_mut()) else {
            return false;
        };
        if theme.get("name").and_then(|v| v.as_str()) != Some("default") {
            return false;
        }
        theme.insert("name".into(), toml::Value::String("zinc".into()));
        info!("Renaming theme 'default' -> 'zinc' in {}", path.display());
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::test_cases::assert_rewrites;

    #[test]
    fn renames_only_the_default_theme() {
        let unchanged = |toml: &'static str| (Some(toml), Some(toml));
        assert_rewrites(
            "config.toml",
            rename_theme,
            &[
                (
                    Some("[theme]\nname = \"default\"\nidle_decay_minutes = 5\n"),
                    Some("[theme]\nname = \"zinc\"\nidle_decay_minutes = 5\n"),
                ),
                unchanged("[theme]\nname = \"empire\"\n"),
                unchanged("[session]\ndefault_tool = \"claude\"\n"),
                (None, None),
            ],
        );
    }
}
