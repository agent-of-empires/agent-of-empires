//! Per-plugin worker stderr logs under `<app_dir>/plugin-workers/`.
//!
//! Files are named `<plugin-id>_<worker-uuid>.log`. Plugin ids never contain
//! `_`, so the first one separates the two parts.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Result;

/// Logs kept per plugin, counting the one a starting worker has just opened.
pub const KEEP_PER_PLUGIN: usize = 5;
/// Age after which an unattributable pre-rename `<uuid>.log` is removed.
const LEGACY_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

pub fn log_path(dir: &Path, plugin_id: &str, worker_id: &str) -> Result<PathBuf> {
    aoe_plugin_api::PluginId::new(plugin_id)?;
    crate::process::worker::log_path(dir, worker_id)?;
    Ok(dir.join(format!("{plugin_id}_{worker_id}.log")))
}

fn plugin_of(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?.strip_suffix(".log")?;
    name.split_once('_').map(|(plugin, _)| plugin)
}

fn modified(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn logs_in(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e == "log"))
                .collect()
        })
        .unwrap_or_default()
}

/// Newest first.
fn logs_for(dir: &Path, plugin_id: &str) -> Vec<PathBuf> {
    let mut logs: Vec<_> = logs_in(dir)
        .into_iter()
        .filter(|p| plugin_of(p) == Some(plugin_id))
        .collect();
    logs.sort_by_key(|p| std::cmp::Reverse(modified(p)));
    logs
}

/// The most recently written log for a plugin, which is the live worker's while one runs.
pub fn latest(dir: &Path, plugin_id: &str) -> Option<PathBuf> {
    logs_for(dir, plugin_id).into_iter().next()
}

/// Removes all but the newest `keep` logs of one plugin. `current` belongs to a
/// worker that is starting or running and is never removed.
pub fn prune_plugin(dir: &Path, plugin_id: &str, current: &Path, keep: usize) {
    let stale = logs_for(dir, plugin_id)
        .into_iter()
        .filter(|p| p != current)
        .skip(keep.saturating_sub(1));
    for path in stale {
        remove(&path);
    }
}

/// Removes old logs written before names carried the plugin id. Workers die with
/// the daemon, so a log that has not been written for a week is not live.
pub fn prune_legacy(dir: &Path) {
    let cutoff = SystemTime::now()
        .checked_sub(LEGACY_MAX_AGE)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    for path in logs_in(dir) {
        let legacy = path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| uuid::Uuid::try_parse(s).is_ok());
        if legacy && modified(&path) < cutoff {
            remove(&path);
        }
    }
}

fn remove(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        tracing::warn!(target: "plugin.host", path = %path.display(), "failed to remove old worker log: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    fn touch(dir: &Path, name: &str, age_secs: u64) -> PathBuf {
        let path = dir.join(name);
        let file = File::create(&path).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(age_secs))
            .unwrap();
        path
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn log_path_embeds_plugin_id() {
        let dir = Path::new("/d");
        let path = log_path(dir, "acme.review-helper", "abc-123").unwrap();
        assert_eq!(path, dir.join("acme.review-helper_abc-123.log"));
        assert_eq!(plugin_of(&path), Some("acme.review-helper"));
        assert!(log_path(dir, "acme.x", "../evil").is_err());
        for bad in ["", "../x", "a/b", "a_b"] {
            assert!(log_path(dir, bad, "abc").is_err(), "{bad:?}");
        }
    }

    #[test]
    fn prune_keeps_newest_per_plugin_and_the_current_log() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // The running worker's log is the oldest by mtime but must survive.
        let current = touch(dir, "a.one_current.log", 900);
        for i in 0..4u64 {
            touch(dir, &format!("a.one_{i}.log"), 100 * (i + 1));
        }
        touch(dir, "a.two_x.log", 5000);
        touch(dir, "a.one-extra_y.log", 5000);

        prune_plugin(dir, "a.one", &current, 3);

        assert_eq!(
            names(dir),
            [
                "a.one-extra_y.log",
                "a.one_0.log",
                "a.one_1.log",
                "a.one_current.log",
                "a.two_x.log"
            ]
        );
        assert_eq!(latest(dir, "a.one").unwrap(), dir.join("a.one_0.log"));
        assert_eq!(latest(dir, "missing"), None);
    }

    #[test]
    fn prune_legacy_removes_only_old_unattributed_logs() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let week = LEGACY_MAX_AGE.as_secs();
        let old = "11111111-1111-4111-8111-111111111111.log";
        let fresh = "22222222-2222-4222-8222-222222222222.log";
        touch(dir, old, week + 60);
        touch(dir, fresh, 60);
        touch(dir, "a.one_old.log", week + 60);
        touch(dir, "notes.log", week + 60);
        touch(dir, "note.txt", week + 60);

        prune_legacy(dir);

        assert_eq!(
            names(dir),
            [fresh, "a.one_old.log", "note.txt", "notes.log"]
        );
    }
}
