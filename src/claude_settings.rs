//! Read-only access to Claude Code's user settings.
//!
//! Used to detect when the user has opted into Claude Code's fullscreen
//! (alt-screen) renderer via `/tui fullscreen`, so the web client can
//! skip mobile workarounds that target the default main-screen renderer.

use std::path::{Path, PathBuf};

fn user_settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("settings.json"))
}

type Stamp = (PathBuf, std::time::SystemTime, u64);

/// Last parse, valid while the file's path, mtime and length are unchanged.
static CACHE: std::sync::Mutex<Option<(Stamp, bool)>> = std::sync::Mutex::new(None);

/// True when the user has set Claude Code's `tui` setting to `"fullscreen"`
/// in `~/.claude/settings.json`. Any other value, missing file, or parse
/// error returns false. Re-reads the file only when it changed.
pub fn read_tui_fullscreen() -> bool {
    let Some(path) = user_settings_path() else {
        return false;
    };
    let Some(stamp) = std::fs::metadata(&path)
        .ok()
        .and_then(|m| Some((path.clone(), m.modified().ok()?, m.len())))
    else {
        return false;
    };
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((cached, value)) = cache.as_ref() {
        if *cached == stamp {
            return *value;
        }
    }
    let Some(value) = read_tui_fullscreen_at(&path) else {
        return false;
    };
    *cache = Some((stamp, value));
    value
}

/// `None` when the file cannot be read, so the caller does not cache a
/// failure that a permission change would fix without touching mtime or length.
fn read_tui_fullscreen_at(path: &Path) -> Option<bool> {
    let contents = std::fs::read_to_string(path).ok()?;
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return Some(false);
    };
    Some(value.get("tui").and_then(|v| v.as_str()) == Some("fullscreen"))
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
        assert_eq!(read_tui_fullscreen_at(path), None);
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
            assert_eq!(
                read_tui_fullscreen_at(f.path()),
                Some(expected),
                "{contents}"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn cached_read_reparses_only_when_the_file_changes() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".claude");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let _guard = crate::session::test_support::isolate_app_dir_at(home.path());

        std::fs::write(&path, r#"{"tui":"fullscreen"}"#).unwrap();
        assert!(read_tui_fullscreen());

        // Same length and mtime: a re-read would see "defaultxxx" and return false.
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&path, r#"{"tui":"defaultxxx"}"#).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert!(read_tui_fullscreen(), "unchanged stamp must not re-read");

        std::fs::write(&path, r#"{"tui":"default"}"#).unwrap();
        assert!(!read_tui_fullscreen(), "changed length must re-read");
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn unreadable_file_is_not_cached() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".claude");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let _guard = crate::session::test_support::isolate_app_dir_at(home.path());

        std::fs::write(&path, r#"{"tui":"fullscreen"}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_to_string(&path).is_ok() {
            return; // privileged user ignores the mode bits
        }
        assert!(!read_tui_fullscreen());

        // chmod leaves mtime and length alone.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_tui_fullscreen(), "a failed read must not be cached");
    }
}
