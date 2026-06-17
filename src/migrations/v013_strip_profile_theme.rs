//! Migration v013: strip per-profile theme overrides.
//!
//! The theme became a single global preference: one theme paints every surface
//! (TUI boot, Settings close, tmux status bar, web `/api/theme/current`)
//! regardless of which session profile is active. Before that, the web
//! dashboard's theme picker wrote `name` / `color_mode` into the *active
//! profile's* `config.toml`, while the TUI wrote them to the global config and
//! booted from it. A profile-level theme then shadowed the global pick on every
//! Settings open/close, flipping the theme (e.g. empire -> rose-pine) until the
//! next restart.
//!
//! This removes `name` and `color_mode` from the `[theme]` table of every
//! `profiles/*/config.toml`, leaving the global `config.toml` (the authoritative
//! theme) untouched. `idle_decay_minutes` stays profile-overridable, so only
//! those two keys are pulled. Idempotent: a profile with no theme override is
//! left alone.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;
use tracing::{debug, info};

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    let profiles_dir = app_dir.join("profiles");
    if !profiles_dir.exists() {
        debug!("No profiles dir; nothing to strip for v013");
        return Ok(());
    }
    for entry in fs::read_dir(&profiles_dir)? {
        let entry = entry?;
        if entry.path().is_dir() {
            strip_profile_theme(&entry.path().join("config.toml"))?;
        }
    }
    Ok(())
}

/// Keys pulled from a profile's `[theme]` table. `idle_decay_minutes` stays
/// profile-overridable and is intentionally not listed.
const GLOBAL_THEME_KEYS: &[&str] = &["name", "color_mode"];

fn strip_profile_theme(path: &Path) -> Result<()> {
    if !path.exists() {
        debug!("Profile config {} does not exist, skipping", path.display());
        return Ok(());
    }
    let content = fs::read_to_string(path)?;
    let mut doc: toml::Table = content
        .parse()
        .with_context(|| format!("Failed to parse {} during v013 migration", path.display()))?;

    let Some(theme) = doc.get_mut("theme").and_then(|t| t.as_table_mut()) else {
        return Ok(());
    };

    let mut removed = Vec::new();
    for key in GLOBAL_THEME_KEYS {
        if theme.remove(*key).is_some() {
            removed.push(*key);
        }
    }
    if removed.is_empty() {
        return Ok(());
    }
    // Drop an emptied [theme] table so the file doesn't keep a dangling header.
    if theme.is_empty() {
        doc.remove("theme");
    }

    info!(
        "Stripping global-only theme keys {:?} from profile config {} (theme is now global)",
        removed,
        path.display()
    );
    let new_content = toml::to_string_pretty(&doc)?;
    crate::session::atomic_write(path, new_content.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
