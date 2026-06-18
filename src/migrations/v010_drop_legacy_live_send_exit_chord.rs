//! Migration v010: Drop legacy/in-dev `live_send_exit_chord` values.
//!
//! Two specific values became the default at different points and
//! ended up baked into user configs (settings TUI's save writes the
//! full in-memory Config, including defaults that haven't been touched
//! by the user):
//!
//! - `"C-q,C-]"`: 1.9.0 shipped this as the default. `Ctrl+]` was
//!   pulled from the default after reports that several terminals on
//!   macOS silently swallow it.
//! - `"C-q,C-\\"`: tried as a replacement default in development.
//!   `Ctrl+\` also silently fails on at least one macOS terminal/
//!   keyboard combination, so it was reverted before release.
//!
//! Either value, baked into a saved config, leaves the footer's live-
//! mode banner advertising a chord that doesn't actually exit. This
//! migration drops the field when it matches one of those two values
//! exactly so the new default (`C-q`) takes effect on next launch.
//! User-customized lists (anything else) are left alone, even if they
//! happen to include `C-]` or `C-\` alongside other chords; removing
//! a chord the user added deliberately would surprise them.

use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;
use tracing::{debug, info};

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

/// Normalize a chord-list string for comparison against the known
/// stuck defaults. tmux-style specs are case-insensitive on the
/// modifier and the letter; the comma-separated form tolerates
/// whitespace around each piece. Long-form modifier names
/// (`Ctrl+`, `Ctrl-`) collapse to the short form so equivalent
/// representations are caught.
fn normalize(spec: &str) -> String {
    spec.chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| c.to_ascii_lowercase())
        .collect::<String>()
        .replace("ctrl+", "c-")
        .replace("ctrl-", "c-")
}

/// Known stuck defaults that this migration should drop. Adding more
/// here as future defaults turn out not to work is the right place;
/// migration tests below cover each addition.
const STUCK_DEFAULTS: &[&str] = &["c-q,c-]", "c-q,c-\\"];

fn is_stuck_default(value: &str) -> bool {
    let normalized = normalize(value);
    STUCK_DEFAULTS.contains(&normalized.as_str())
}

fn migrate_config_file(path: &PathBuf) -> Result<()> {
    if !path.exists() {
        debug!("Config file {} does not exist, skipping", path.display());
        return Ok(());
    }

    let content = fs::read_to_string(path)?;
    let mut doc: toml::Table = content
        .parse()
        .with_context(|| format!("Failed to parse {} during v010 migration", path.display()))?;

    let Some(session) = doc.get_mut("session").and_then(|s| s.as_table_mut()) else {
        debug!("No [session] section in {}, skipping", path.display());
        return Ok(());
    };

    let Some(chord_value) = session.get("live_send_exit_chord").cloned() else {
        debug!("No live_send_exit_chord in {}, skipping", path.display());
        return Ok(());
    };

    let Some(chord_str) = chord_value.as_str() else {
        debug!(
            "live_send_exit_chord in {} is not a string, skipping",
            path.display()
        );
        return Ok(());
    };

    if !is_stuck_default(chord_str) {
        debug!(
            "live_send_exit_chord in {} is customized ({:?}), leaving alone",
            path.display(),
            chord_str
        );
        return Ok(());
    }

    info!(
        "Dropping stuck live_send_exit_chord = {:?} from {} (chord removed from default)",
        chord_str,
        path.display()
    );
    session.remove("live_send_exit_chord");

    let new_content = toml::to_string_pretty(&doc)?;
    crate::session::atomic_write(path, new_content.as_bytes())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(content: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, content).unwrap();
        (dir, path)
    }

    #[test]
    fn shipped_1_9_0_default_is_dropped() {
        let (_dir, path) = write(
            r#"
[session]
live_send_exit_chord = "C-q,C-]"
default_tool = "claude"
"#,
        );
        migrate_config_file(&path).unwrap();
        let result: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert!(result["session"]
            .as_table()
            .unwrap()
            .get("live_send_exit_chord")
            .is_none());
        // Other session fields untouched.
        assert_eq!(result["session"]["default_tool"].as_str(), Some("claude"));
    }

    #[test]
    fn in_dev_backslash_default_is_dropped() {
        let (_dir, path) = write(
            r#"
[session]
live_send_exit_chord = 'C-q,C-\'
"#,
        );
        migrate_config_file(&path).unwrap();
        let result: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert!(result["session"]
            .as_table()
            .unwrap()
            .get("live_send_exit_chord")
            .is_none());
    }

    #[test]
    fn long_form_modifier_names_are_recognized() {
        let (_dir, path) = write(
            r#"
[session]
live_send_exit_chord = "Ctrl+Q, Ctrl+]"
"#,
        );
        migrate_config_file(&path).unwrap();
        let result: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert!(result["session"]
            .as_table()
            .unwrap()
            .get("live_send_exit_chord")
            .is_none());
    }

    #[test]
    fn customized_chord_list_is_left_alone() {
        // User added F12 on top of a stuck default. They clearly care
        // about the chord list; don't touch it.
        let (_dir, path) = write(
            r#"
[session]
live_send_exit_chord = "C-q,C-],F12"
"#,
        );
        migrate_config_file(&path).unwrap();
        let result: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(
            result["session"]["live_send_exit_chord"].as_str(),
            Some("C-q,C-],F12")
        );
    }

    #[test]
    fn current_default_value_is_left_alone() {
        // User explicitly set the new default. Nothing to clean.
        let (_dir, path) = write(
            r#"
[session]
live_send_exit_chord = "C-q"
"#,
        );
        migrate_config_file(&path).unwrap();
        let result: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(
            result["session"]["live_send_exit_chord"].as_str(),
            Some("C-q")
        );
    }

    #[test]
    fn unrelated_custom_value_is_left_alone() {
        // F12-only is a legitimate user choice; leave it alone.
        let (_dir, path) = write(
            r#"
[session]
live_send_exit_chord = "F12"
"#,
        );
        migrate_config_file(&path).unwrap();
        let result: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(
            result["session"]["live_send_exit_chord"].as_str(),
            Some("F12")
        );
    }

    #[test]
    fn missing_field_is_noop() {
        let (_dir, path) = write(
            r#"
[session]
default_tool = "claude"
"#,
        );
        migrate_config_file(&path).unwrap();
        let result: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(result["session"]["default_tool"].as_str(), Some("claude"));
    }

    #[test]
    fn no_session_section_is_noop() {
        let (_dir, path) = write(
            r#"
[updates]
notify_in_cli = true
"#,
        );
        migrate_config_file(&path).unwrap();
        let result: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(result["updates"]["notify_in_cli"].as_bool(), Some(true));
    }

    #[test]
    fn nonexistent_file_is_noop() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.toml");
        migrate_config_file(&path).unwrap();
    }
}
