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
    use std::fs;

    fn write(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, content).unwrap();
        (dir, path)
    }

    #[test]
    fn strips_name_and_color_mode_but_keeps_idle_decay() {
        let (_dir, path) = write(
            r#"
[theme]
name = "rose-pine"
color_mode = "palette"
idle_decay_minutes = 5
"#,
        );
        strip_profile_theme(&path).unwrap();
        let result: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        let theme = result.get("theme").and_then(|t| t.as_table()).unwrap();
        assert!(theme.get("name").is_none(), "name should be stripped");
        assert!(
            theme.get("color_mode").is_none(),
            "color_mode should be stripped"
        );
        assert_eq!(
            theme.get("idle_decay_minutes").and_then(|v| v.as_integer()),
            Some(5),
            "idle_decay_minutes stays profile-overridable"
        );
    }

    #[test]
    fn drops_table_when_only_global_keys_present() {
        let (_dir, path) = write(
            r#"
[theme]
name = "rose-pine"

[session]
default_tool = "claude"
"#,
        );
        strip_profile_theme(&path).unwrap();
        let result: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert!(
            result.get("theme").is_none(),
            "an emptied [theme] table is removed"
        );
        // Unrelated sections are untouched.
        assert!(result.get("session").is_some());
    }

    #[test]
    fn idempotent_when_no_theme_override() {
        let (_dir, path) = write(
            r#"
[session]
default_tool = "claude"
"#,
        );
        let before = fs::read_to_string(&path).unwrap();
        strip_profile_theme(&path).unwrap();
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(before, after, "no theme override means no rewrite");
    }

    #[test]
    fn missing_file_is_a_noop() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("nope.toml");
        assert!(strip_profile_theme(&path).is_ok());
    }
}
