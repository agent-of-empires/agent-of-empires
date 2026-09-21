//! Migration v010: drop `live_send_exit_chord` when it holds one of the two
//! chords that were briefly the default, `"C-q,C-]"` and `"C-q,C-\\"`. Both
//! are swallowed by some macOS terminals, and the settings TUI bakes untouched
//! defaults into saved configs, so the footer advertised an exit that did not
//! work. Any other value is a user choice and stays.

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

/// Fold a chord spec to the form [`STUCK_DEFAULTS`] is written in: the
/// modifier and letter are case-insensitive, whitespace around each piece is
/// tolerated, and `Ctrl+` / `Ctrl-` are the long spelling of `c-`.
fn normalize(spec: &str) -> String {
    spec.chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| c.to_ascii_lowercase())
        .collect::<String>()
        .replace("ctrl+", "c-")
        .replace("ctrl-", "c-")
}

/// The defaults this drops. A future default that turns out not to work is
/// added here, with a case in the tests below.
const STUCK_DEFAULTS: &[&str] = &["c-q,c-]", "c-q,c-\\"];

fn is_stuck_default(value: &str) -> bool {
    let normalized = normalize(value);
    STUCK_DEFAULTS.contains(&normalized.as_str())
}

fn migrate_config_file(path: &Path) -> Result<()> {
    config_file::rewrite_strict(path, "v010", |doc| {
        let Some(session) = doc.get_mut("session").and_then(|s| s.as_table_mut()) else {
            return false;
        };
        let stuck = session
            .get("live_send_exit_chord")
            .and_then(|v| v.as_str())
            .is_some_and(is_stuck_default);
        if !stuck {
            return false;
        }
        info!(
            "Dropping stuck live_send_exit_chord from {} (chord removed from default)",
            path.display()
        );
        session.remove("live_send_exit_chord");
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

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
