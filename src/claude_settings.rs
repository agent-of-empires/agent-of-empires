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
    fn missing_file_returns_false() {
        let path = std::path::Path::new("/nonexistent/hmp-test/settings.json");
        assert!(!read_tui_fullscreen_at(path));
    }

    #[test]
    fn fullscreen_returns_true() {
        let f = write_settings(r#"{"tui": "fullscreen"}"#);
        assert!(read_tui_fullscreen_at(f.path()));
    }

    #[test]
    fn default_returns_false() {
        let f = write_settings(r#"{"tui": "default"}"#);
        assert!(!read_tui_fullscreen_at(f.path()));
    }

    #[test]
    fn missing_key_returns_false() {
        let f = write_settings(r#"{"theme": "dark"}"#);
        assert!(!read_tui_fullscreen_at(f.path()));
    }

    #[test]
    fn malformed_json_returns_false() {
        let f = write_settings("{not valid json");
        assert!(!read_tui_fullscreen_at(f.path()));
    }

    #[test]
    fn fullscreen_among_other_keys_returns_true() {
        let f = write_settings(r#"{"theme": "dark", "tui": "fullscreen", "model": "sonnet"}"#);
        assert!(read_tui_fullscreen_at(f.path()));
    }

    #[test]
    fn non_string_tui_returns_false() {
        let f = write_settings(r#"{"tui": true}"#);
        assert!(!read_tui_fullscreen_at(f.path()));
    }
}
