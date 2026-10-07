//! Read-only access to Claude Code's user settings.
//!
//! Used to detect when the user has opted into Claude Code's fullscreen
//! (alt-screen) renderer via `/tui fullscreen`, so the web client can
//! skip mobile workarounds that target the default main-screen renderer.

use std::path::{Path, PathBuf};

fn user_settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("settings.json"))
}

/// True when the user has set Claude Code's `tui` setting to `"fullscreen"`
/// in `~/.claude/settings.json`. Any other value, missing file, or parse
/// error returns false.
pub fn read_tui_fullscreen() -> bool {
    user_settings_path()
        .map(|p| read_tui_fullscreen_at(&p))
        .unwrap_or(false)
}

fn read_tui_fullscreen_at(path: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return false;
    };
    value.get("tui").and_then(|v| v.as_str()) == Some("fullscreen")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_settings(contents: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        f
    }

    #[test]
    fn read_tui_fullscreen_only_for_a_fullscreen_string() {
        let path = std::path::Path::new("/nonexistent/aoe-test/settings.json");
        assert!(!read_tui_fullscreen_at(path));
        for (contents, expected) in [
            (r#"{"tui": "fullscreen"}"#, true),
            (
                r#"{"theme": "dark", "tui": "fullscreen", "model": "sonnet"}"#,
                true,
            ),
            (r#"{"tui": "default"}"#, false),
            (r#"{"theme": "dark"}"#, false),
            ("{not valid json", false),
            (r#"{"tui": true}"#, false),
        ] {
            let f = write_settings(contents);
            assert_eq!(read_tui_fullscreen_at(f.path()), expected, "{contents}");
        }
    }
}
