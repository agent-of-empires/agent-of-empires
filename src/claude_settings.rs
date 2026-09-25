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
///
/// The answer is cached against the file's modification time and size. Every
/// session-row projection asks, and the daemon reprojects on a timer, so
/// without this the file is read and JSON-parsed every couple of seconds on an
/// async worker to produce the answer it already had.
pub fn read_tui_fullscreen() -> bool {
    let Some(path) = user_settings_path() else {
        return false;
    };
    let stamp = file_stamp(&path);
    let mut cache = cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Some((cached_stamp, fullscreen)) = *cache {
        if cached_stamp == stamp {
            return fullscreen;
        }
    }
    let fullscreen = read_tui_fullscreen_at(&path);
    *cache = Some((stamp, fullscreen));
    fullscreen
}

/// What the cache compares. `None` for a file that is absent or unreadable,
/// which is itself an answer worth keeping.
type Stamp = Option<(std::time::SystemTime, u64)>;

fn file_stamp(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

fn cache() -> &'static std::sync::Mutex<Option<(Stamp, bool)>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Option<(Stamp, bool)>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
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
        let path = std::path::Path::new("/nonexistent/aoe-test/settings.json");
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
