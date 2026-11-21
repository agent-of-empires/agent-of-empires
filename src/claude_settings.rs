//! Read-only access to Claude Code's user settings.
//!
//! Used to detect when the user has opted into Claude Code's fullscreen
//! (alt-screen) renderer via `/tui fullscreen`, so the web client can
//! skip mobile workarounds that target the default main-screen renderer.

use std::io::Read;
use std::path::{Path, PathBuf};

fn user_settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("settings.json"))
}

#[cfg(unix)]
type Identity = (u64, i64, i64);
#[cfg(not(unix))]
type Identity = ();

/// Path, mtime, length and, on unix, inode and ctime: a chmod or a replacement that
/// keeps mtime and length still changes the stamp.
type Stamp = (PathBuf, std::time::SystemTime, u64, Identity);

#[cfg(unix)]
fn identity(meta: &std::fs::Metadata) -> Identity {
    use std::os::unix::fs::MetadataExt;
    (meta.ino(), meta.ctime(), meta.ctime_nsec())
}

#[cfg(not(unix))]
fn identity(_: &std::fs::Metadata) -> Identity {}

/// Last parse, valid while the stamp of the opened file is unchanged.
static CACHE: std::sync::Mutex<Option<(Stamp, bool)>> = std::sync::Mutex::new(None);

/// True when the user has set Claude Code's `tui` setting to `"fullscreen"`
/// in `~/.claude/settings.json`. Any other value, missing file, or parse
/// error returns false. Re-reads the file only when it changed.
pub fn read_tui_fullscreen() -> bool {
    let Some(path) = user_settings_path() else {
        return false;
    };
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    read_tui_fullscreen_at(&path, &mut cache)
}

/// Stamps and reads the same open handle. An unopenable or unreadable file
/// returns false and drops the entry, so it is retried once access returns.
fn read_tui_fullscreen_at(path: &Path, cache: &mut Option<(Stamp, bool)>) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        *cache = None;
        return false;
    };
    let Some(stamp) = file.metadata().ok().and_then(|m| {
        Some((
            path.to_path_buf(),
            m.modified().ok()?,
            m.len(),
            identity(&m),
        ))
    }) else {
        *cache = None;
        return false;
    };
    if let Some((cached, value)) = cache.as_ref() {
        if *cached == stamp {
            return *value;
        }
    }
    let mut contents = String::new();
    if file.read_to_string(&mut contents).is_err() {
        *cache = None;
        return false;
    }
    let value = serde_json::from_str::<serde_json::Value>(&contents)
        .is_ok_and(|v| v.get("tui").and_then(|v| v.as_str()) == Some("fullscreen"));
    *cache = Some((stamp, value));
    value
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
        assert!(!read_tui_fullscreen_at(path, &mut None));
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
                read_tui_fullscreen_at(f.path(), &mut None),
                expected,
                "{contents}"
            );
        }
    }

    #[test]
    fn cached_read_reparses_only_when_the_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut cache = None;

        std::fs::write(&path, r#"{"tui":"fullscreen"}"#).unwrap();
        assert!(read_tui_fullscreen_at(&path, &mut cache));

        // Poison the entry: only a cache hit can return it.
        cache.as_mut().unwrap().1 = false;
        assert!(
            !read_tui_fullscreen_at(&path, &mut cache),
            "an unchanged file must not be re-read"
        );

        std::fs::write(&path, r#"{"tui": "fullscreen"}"#).unwrap();
        assert!(
            read_tui_fullscreen_at(&path, &mut cache),
            "a changed file must be re-read"
        );
    }

    #[cfg(unix)]
    #[test]
    fn warm_entry_does_not_survive_a_permission_change() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut cache = None;

        std::fs::write(&path, r#"{"tui":"fullscreen"}"#).unwrap();
        assert!(read_tui_fullscreen_at(&path, &mut cache));

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&path).is_ok() {
            return; // privileged user ignores the mode bits
        }
        assert!(
            !read_tui_fullscreen_at(&path, &mut cache),
            "a warm entry must not outlive read access"
        );

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            read_tui_fullscreen_at(&path, &mut cache),
            "a failed read must not be cached"
        );
    }

    #[cfg(unix)]
    #[test]
    fn same_length_replacement_with_the_same_mtime_is_re_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut cache = None;

        std::fs::write(&path, r#"{"tui":"fullscreen"}"#).unwrap();
        assert!(read_tui_fullscreen_at(&path, &mut cache));

        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        let replacement = dir.path().join("replacement.json");
        std::fs::write(&replacement, r#"{"tui":"fullscxxxx"}"#).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert!(!read_tui_fullscreen_at(&path, &mut cache));
    }
}
