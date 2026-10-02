//! Compatibility API for cross-profile move records stored in the lifecycle journal.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Bump when the entry shape changes. Entries written by an older version
/// carry no arbitration authority (see [`MoveJournalEntry::is_current`]).
pub(crate) const MOVE_JOURNAL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MoveJournalEntry {
    pub(crate) version: u32,
    /// Sorted session ids covered by one move batch.
    pub(crate) ids: Vec<String>,
    pub(crate) source_profile: String,
    pub(crate) target_profile: String,
    /// Absolute paths of both profiles' `sessions.json`, as recorded at
    /// journal-write time. Recovery re-checks them against the live stores.
    pub(crate) source_sessions_path: PathBuf,
    pub(crate) target_sessions_path: PathBuf,
    pub(crate) group_move_source_path: String,
    pub(crate) group_move_target_path: String,
    pub(crate) group_move_subtree: bool,
    pub(crate) created_at_epoch_ms: u64,
}

impl MoveJournalEntry {
    pub(crate) fn is_current(&self) -> bool {
        self.version == MOVE_JOURNAL_VERSION
    }
}

/// `.lifecycle-journal/` inside the directory holding `sessions_path`.
#[cfg(test)]
fn journal_dir_for(sessions_path: &Path) -> PathBuf {
    sessions_path
        .parent()
        .unwrap_or(sessions_path)
        .join(".lifecycle-journal")
}

#[cfg(test)]
fn legacy_journal_dir_for(sessions_path: &Path) -> PathBuf {
    sessions_path
        .parent()
        .unwrap_or(sessions_path)
        .join(".move-journal")
}

/// Persist one entry next to the source profile's `sessions.json` and return its path.
pub(crate) fn record(entry: &MoveJournalEntry, source_sessions_path: &Path) -> Result<PathBuf> {
    super::lifecycle_journal::record_move(entry, source_sessions_path)
}

#[cfg(test)]
fn record_with_sync<S>(
    entry: &MoveJournalEntry,
    source_sessions_path: &Path,
    sync: S,
) -> Result<PathBuf>
where
    S: FnMut(&Path) -> Result<()>,
{
    super::lifecycle_journal::record_move_with_sync(entry, source_sessions_path, sync)
}

/// Nanosecond creation order encoded in record's filename. Used only as a
/// durable tie-breaker when two entries share the millisecond JSON timestamp.
pub(crate) fn file_created_at_nanos(path: &Path) -> Option<u128> {
    path.file_stem()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("move-"))
        .and_then(|name| name.split_once('-').map(|(nanos, _)| nanos))
        .and_then(|nanos| nanos.parse().ok())
}

/// Delete one consumed entry and sync its parent directory so the removal
/// itself is durable. Idempotent: a missing file is already consumed.
pub(crate) fn consume(path: &Path) -> Result<()> {
    super::lifecycle_journal::consume(path)
}

/// Result of scanning every loaded profile's journal directory.
pub(crate) struct ScanResult {
    pub(crate) entries: Vec<(PathBuf, std::result::Result<MoveJournalEntry, String>)>,
    pub(crate) unreadable_dirs: Vec<(PathBuf, String)>,
}

/// Every journal file under each given profile directory paired with its parse outcome.
pub(crate) fn scan(sessions_paths: impl IntoIterator<Item = PathBuf>) -> ScanResult {
    let result = super::lifecycle_journal::scan_moves(sessions_paths);
    ScanResult {
        entries: result.entries,
        unreadable_dirs: result.unreadable_dirs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn entry(source: &Path, target: &Path) -> MoveJournalEntry {
        MoveJournalEntry {
            version: MOVE_JOURNAL_VERSION,
            ids: vec!["session-id".to_string()],
            source_profile: "source".to_string(),
            target_profile: "target".to_string(),
            source_sessions_path: source.to_path_buf(),
            target_sessions_path: target.to_path_buf(),
            group_move_source_path: "work".to_string(),
            group_move_target_path: "moved".to_string(),
            group_move_subtree: false,
            created_at_epoch_ms: 1,
        }
    }

    #[test]
    fn record_requires_profile_parent_barrier_before_writing_entry() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source/sessions.json");
        let target = temp.path().join("target/sessions.json");
        fs::create_dir_all(source.parent().unwrap())?;
        fs::create_dir_all(target.parent().unwrap())?;
        let expected_dir = journal_dir_for(&source);
        let mut calls = Vec::new();

        let error = record_with_sync(&entry(&source, &target), &source, |path| {
            calls.push(path.to_path_buf());
            Err(anyhow::anyhow!("forced profile-parent barrier failure"))
        })
        .expect_err("journal record must fail before an unverified directory can be used");

        assert!(error.to_string().contains("journal directory"));
        assert_eq!(calls, vec![expected_dir.clone()]);
        assert!(
            expected_dir.exists(),
            "directory was created before its barrier"
        );
        assert!(
            fs::read_dir(expected_dir)?.next().is_none(),
            "no entry may be written before the parent barrier"
        );
        Ok(())
    }

    #[test]
    fn scan_separates_directory_failures_from_entry_results() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let sessions = temp.path().join("profile/sessions.json");
        fs::create_dir_all(sessions.parent().unwrap())?;
        let journal_dir = legacy_journal_dir_for(&sessions);
        fs::write(&journal_dir, b"not-a-directory")?;

        let result = scan([sessions]);

        assert!(result.entries.is_empty());
        assert_eq!(result.unreadable_dirs.len(), 1);
        assert_eq!(result.unreadable_dirs[0].0, journal_dir);
        Ok(())
    }

    #[test]
    fn scan_reads_legacy_move_records() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source/sessions.json");
        let target = temp.path().join("target/sessions.json");
        fs::create_dir_all(source.parent().unwrap())?;
        fs::create_dir_all(target.parent().unwrap())?;
        let legacy_dir = legacy_journal_dir_for(&source);
        fs::create_dir_all(&legacy_dir)?;
        let path = legacy_dir.join("move-1-1.json");
        fs::write(&path, serde_json::to_vec(&entry(&source, &target))?)?;

        let result = scan([source]);

        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].0, path);
        assert_eq!(result.entries[0].1.as_ref().unwrap().ids[0], "session-id");
        Ok(())
    }

    #[test]
    fn scan_rejects_unsupported_legacy_move_record_versions() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source/sessions.json");
        let target = temp.path().join("target/sessions.json");
        fs::create_dir_all(source.parent().unwrap())?;
        fs::create_dir_all(target.parent().unwrap())?;
        let legacy_dir = legacy_journal_dir_for(&source);
        fs::create_dir_all(&legacy_dir)?;
        let path = legacy_dir.join("move-unsupported.json");
        let mut unsupported = entry(&source, &target);
        unsupported.version = MOVE_JOURNAL_VERSION + 1;
        fs::write(&path, serde_json::to_vec(&unsupported)?)?;

        let result = scan([source]);

        let error = result.entries[0].1.as_ref().unwrap_err();
        assert!(error.contains("move journal version 2 is not supported"));
        Ok(())
    }
}
