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
    use crate::migrations::test_cases::assert_rewrites;

    #[test]
    fn drops_only_stuck_default_exit_chords() {
        let unchanged = |toml: &'static str| (Some(toml), Some(toml));
        let dropped =
            |before: &'static str| (Some(before), Some("[session]\ndefault_tool = \"claude\"\n"));
        assert_rewrites(
            "config.toml",
            migrate_config_file,
            &[
                dropped("[session]\nlive_send_exit_chord = \"C-q,C-]\"\ndefault_tool = \"claude\"\n"),
                dropped("[session]\nlive_send_exit_chord = 'C-q,C-\\'\ndefault_tool = \"claude\"\n"),
                dropped("[session]\nlive_send_exit_chord = \"Ctrl+Q, Ctrl+]\"\ndefault_tool = \"claude\"\n"),
                // A customised list, the current default and an unrelated value are the user's.
                unchanged("[session]\nlive_send_exit_chord = \"C-q,C-],F12\"\n"),
                unchanged("[session]\nlive_send_exit_chord = \"C-q\"\n"),
                unchanged("[session]\nlive_send_exit_chord = \"F12\"\n"),
                unchanged("[session]\ndefault_tool = \"claude\"\n"),
                unchanged("[updates]\nnotify_in_cli = true\n"),
                (None, None),
            ],
        );
    }
}
