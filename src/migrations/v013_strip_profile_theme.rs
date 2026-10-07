//! Migration v013: strip `name` and `color_mode` from the `[theme]` table of
//! every `profiles/*/config.toml`. The theme is a single global preference
//! now, and a profile override shadowed the global pick on every Settings
//! open and close. `idle_decay_minutes` stays profile-overridable, and the
//! global config is left untouched.

use super::config_file;
use anyhow::Result;
use std::path::Path;
use tracing::info;

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    for path in config_file::profile_configs(&app_dir)? {
        strip_profile_theme(&path)?;
    }
    Ok(())
}

/// Keys pulled from a profile's `[theme]` table. `idle_decay_minutes` stays
/// profile-overridable and is intentionally not listed.
const GLOBAL_THEME_KEYS: &[&str] = &["name", "color_mode"];

fn strip_profile_theme(path: &Path) -> Result<()> {
    config_file::rewrite_strict(path, "v013", |doc| {
        let Some(theme) = doc.get_mut("theme").and_then(|t| t.as_table_mut()) else {
            return false;
        };
        let removed: Vec<_> = GLOBAL_THEME_KEYS
            .iter()
            .filter(|key| theme.remove(**key).is_some())
            .collect();
        if removed.is_empty() {
            return false;
        }
        // Drop an emptied [theme] so the file keeps no dangling header.
        if theme.is_empty() {
            doc.remove("theme");
        }
        info!(
            "Stripping global-only theme keys {:?} from profile config {} (theme is now global)",
            removed,
            path.display()
        );
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::test_cases::assert_rewrites;

    #[test]
    fn strips_global_only_theme_keys_from_profiles() {
        let no_theme = "[session]\ndefault_tool = \"claude\"\n";
        assert_rewrites(
            "config.toml",
            strip_profile_theme,
            &[
                // idle_decay_minutes stays profile-overridable.
                (
                    Some("[theme]\nname = \"rose-pine\"\ncolor_mode = \"palette\"\nidle_decay_minutes = 5\n"),
                    Some("[theme]\nidle_decay_minutes = 5\n"),
                ),
                // An emptied [theme] table is removed.
                (
                    Some("[theme]\nname = \"rose-pine\"\n\n[session]\ndefault_tool = \"claude\"\n"),
                    Some(no_theme),
                ),
                (Some(no_theme), Some(no_theme)),
                (None, None),
            ],
        );
    }
}
