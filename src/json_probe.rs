//! Daemon-side probe of one string value in a JSON file under the user's home.
//!
//! Agent profiles declare probes (for example Claude Code's `/tui` setting) so
//! clients get the evaluated result and never read the file themselves.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};

/// True when the string at `pointer` (RFC 6901) in the JSON file at `path`
/// (relative to the home directory) equals `equals`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JsonProbe {
    pub path: &'static str,
    pub pointer: &'static str,
    pub equals: &'static str,
}

/// `path` under the home directory, or `None` for an absolute path or one with `..`.
fn home_path(path: &str) -> Option<PathBuf> {
    let rel = Path::new(path);
    if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(dirs::home_dir()?.join(rel))
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

type Entry = Option<(Stamp, bool)>;

/// Last result per probe, valid while the stamp of the opened file is unchanged.
static CACHE: LazyLock<Mutex<HashMap<JsonProbe, Entry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Evaluate `probe`. A missing or invalid path, missing file, parse error or
/// other value returns false. Re-reads the file only when it changed.
pub fn matches(probe: &JsonProbe) -> bool {
    let Some(path) = home_path(probe.path) else {
        return false;
    };
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    probe_at(&path, probe, cache.entry(*probe).or_default())
}

/// Stamps and reads the same open handle. An unopenable or unreadable file
/// returns false and drops the entry, so it is retried once access returns.
fn probe_at(path: &Path, probe: &JsonProbe, cache: &mut Entry) -> bool {
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
        .is_ok_and(|v| v.pointer(probe.pointer).and_then(|v| v.as_str()) == Some(probe.equals));
    *cache = Some((stamp, value));
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    const TUI: JsonProbe = JsonProbe {
        path: ".claude/settings.json",
        pointer: "/tui",
        equals: "fullscreen",
    };

    fn write_settings(contents: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        f
    }

    #[test]
    fn probe_matches_only_the_expected_string() {
        let path = std::path::Path::new("/nonexistent/aoe-test/settings.json");
        assert!(!probe_at(path, &TUI, &mut None));
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
            assert_eq!(probe_at(f.path(), &TUI, &mut None), expected, "{contents}");
        }
    }

    #[test]
    fn probe_paths_must_stay_under_home() {
        for bad in ["/etc/passwd", "../x.json", ".claude/../../x.json"] {
            assert!(home_path(bad).is_none(), "{bad}");
        }
        let probe = JsonProbe {
            path: "/etc/passwd",
            ..TUI
        };
        assert!(!matches(&probe));
        assert!(home_path(TUI.path).is_some());
    }

    #[test]
    fn cached_read_reparses_only_when_the_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut cache = None;

        std::fs::write(&path, r#"{"tui":"fullscreen"}"#).unwrap();
        assert!(probe_at(&path, &TUI, &mut cache));

        // Poison the entry: only a cache hit can return it.
        cache.as_mut().unwrap().1 = false;
        assert!(
            !probe_at(&path, &TUI, &mut cache),
            "an unchanged file must not be re-read"
        );

        std::fs::write(&path, r#"{"tui": "fullscreen"}"#).unwrap();
        assert!(
            probe_at(&path, &TUI, &mut cache),
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
        assert!(probe_at(&path, &TUI, &mut cache));

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&path).is_ok() {
            return; // privileged user ignores the mode bits
        }
        assert!(
            !probe_at(&path, &TUI, &mut cache),
            "a warm entry must not outlive read access"
        );

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            probe_at(&path, &TUI, &mut cache),
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
        assert!(probe_at(&path, &TUI, &mut cache));

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
        assert!(!probe_at(&path, &TUI, &mut cache));
    }
}
