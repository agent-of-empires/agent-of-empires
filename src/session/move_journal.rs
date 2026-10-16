//! Durable journal for cross-profile session moves.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;

use super::storage::{atomic_write_verified, sync_parent_directory};

/// Bump when the entry shape changes. Entries written by an older version
/// carry no arbitration authority (see [`MoveJournalEntry::is_current`]).
pub(crate) const MOVE_JOURNAL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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

use crate::process::DirectoryIdentity;

fn identity_from_metadata(metadata: &fs::Metadata) -> Result<DirectoryIdentity> {
    anyhow::ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "move journal entry is not a regular file"
    );
    Ok(DirectoryIdentity::from_metadata(metadata)?)
}

pub(crate) fn file_identity(path: &Path) -> Result<DirectoryIdentity> {
    identity_from_metadata(&fs::symlink_metadata(path)?)
}

const JOURNAL_DIR_NAME: &str = ".move-journal";

/// `.move-journal/` inside the directory holding `sessions_path`.
fn journal_dir_for(sessions_path: &Path) -> PathBuf {
    sessions_path
        .parent()
        .unwrap_or(sessions_path)
        .join(JOURNAL_DIR_NAME)
}

/// Persist one entry next to the source profile's `sessions.json` and return its path.
pub(crate) fn record(entry: &MoveJournalEntry, source_sessions_path: &Path) -> Result<PathBuf> {
    super::profile_rename::ensure_move_available(entry)?;
    record_with_sync(entry, source_sessions_path, sync_parent_directory)
}

fn record_with_sync<S>(
    entry: &MoveJournalEntry,
    source_sessions_path: &Path,
    mut sync: S,
) -> Result<PathBuf>
where
    S: FnMut(&Path) -> Result<()>,
{
    let dir = journal_dir_for(source_sessions_path);
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    sync(&dir)
        .with_context(|| format!("journal directory {} was not made durable", dir.display()))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let path = dir.join(format!("move-{nanos}-{}.json", std::process::id()));
    let bytes = serde_json::to_vec_pretty(entry)?;
    atomic_write_verified(&path, &bytes)
        .with_context(|| format!("failed to write move journal {}", path.display()))?;
    sync(&path).with_context(|| format!("move journal {} was not made durable", path.display()))?;
    Ok(path)
}

/// Nanosecond creation order encoded in record's filename. Used only as a
/// durable tie-breaker when two entries share the millisecond JSON timestamp.
pub(crate) fn file_created_at_nanos(path: &Path) -> Option<u128> {
    let (nanos, pid) = path
        .file_stem()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("move-"))
        .and_then(|name| name.split_once('-'))?;
    pid.parse::<u32>().ok()?;
    nanos.parse().ok()
}

/// Delete one consumed entry and sync its parent directory so the removal
/// itself is durable. Idempotent: a missing file is already consumed.
pub(crate) fn consume(path: &Path) -> Result<()> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let entry: MoveJournalEntry = serde_json::from_slice(&bytes)?;
    super::profile_rename::ensure_move_available(&entry)?;
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to remove {}", path.display()))
        }
    }
    sync_parent_directory(path)
        .with_context(|| format!("removal of {} was not made durable", path.display()))
}

/// Result of scanning every loaded profile's journal directory.
pub(crate) struct ScanResult {
    pub(crate) entries: Vec<(PathBuf, std::result::Result<MoveJournalEntry, String>)>,
    pub(crate) unreadable_dirs: Vec<(PathBuf, String)>,
}

/// Every journal file under each given profile directory paired with its parse outcome.
pub(crate) fn scan(sessions_paths: impl IntoIterator<Item = PathBuf>) -> ScanResult {
    let mut result = ScanResult {
        entries: Vec::new(),
        unreadable_dirs: Vec::new(),
    };
    let mut dirs: Vec<PathBuf> = sessions_paths
        .into_iter()
        .map(|sessions_path| journal_dir_for(&sessions_path))
        .collect();
    dirs.sort();
    dirs.dedup();
    for dir in dirs {
        let read_dir = match fs::read_dir(&dir) {
            Ok(read_dir) => read_dir,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                result.unreadable_dirs.push((
                    dir.clone(),
                    format!("failed to list {}: {error}", dir.display()),
                ));
                continue;
            }
        };
        let mut paths = Vec::new();
        for entry in read_dir {
            match entry {
                Ok(entry) => paths.push(entry.path()),
                Err(error) => {
                    result.unreadable_dirs.push((
                        dir.clone(),
                        format!("failed to enumerate {}: {error}", dir.display()),
                    ));
                    paths.clear();
                    break;
                }
            }
        }
        paths.sort();
        for path in paths {
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    result.entries.push((
                        path,
                        Err(format!("failed to inspect move journal entry: {error}")),
                    ));
                    continue;
                }
            };
            let unpublished_atomic_write = metadata.file_type().is_file()
                && !metadata.file_type().is_symlink()
                && path.file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with(super::storage::ATOMIC_WRITE_TEMP_PREFIX)
                        // Older atomic writes used tempfile's default prefix.
                        || name.starts_with(".tmp")
                });
            if unpublished_atomic_write {
                continue;
            }
            let parsed = (|| -> Result<MoveJournalEntry> {
                anyhow::ensure!(
                    path.extension()
                        .is_some_and(|extension| extension == "json"),
                    "unexpected move journal entry"
                );
                anyhow::ensure!(
                    metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
                    "move journal entry is not a regular file"
                );
                let bytes = fs::read(&path).context("failed to read move journal entry")?;
                let entry: MoveJournalEntry =
                    serde_json::from_slice(&bytes).context("malformed move journal entry")?;
                anyhow::ensure!(
                    entry.is_current(),
                    "move journal version {} is not supported (current: {MOVE_JOURNAL_VERSION})",
                    entry.version
                );
                Ok(entry)
            })()
            .map_err(|error| format!("{error:#}"));
            result.entries.push((path, parsed));
        }
    }
    result
}

fn open_matching(path: &Path, expected: &DirectoryIdentity) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options
        .open(path)
        .with_context(|| format!("opening move journal {}", path.display()))?;
    anyhow::ensure!(
        identity_from_metadata(&file.metadata()?)? == *expected,
        "move journal identity changed before update"
    );
    Ok(file)
}

pub(crate) fn sync(path: &Path, expected: &DirectoryIdentity) -> Result<()> {
    open_matching(path, expected)?.sync_all()?;
    sync_parent_directory(path)
}

pub(crate) fn update(
    path: &Path,
    expected: &DirectoryIdentity,
    entry: &MoveJournalEntry,
) -> Result<()> {
    let mut file = open_matching(path, expected)?;
    file.set_len(0)?;
    #[cfg(test)]
    super::storage::test_crash_point("profile-rename-move-after-truncate");
    file.write_all(&serde_json::to_vec_pretty(entry)?)?;
    #[cfg(test)]
    super::storage::test_crash_point("profile-rename-move-after-write");
    file.sync_all()?;
    sync_parent_directory(path)
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
        let journal_dir = journal_dir_for(&sessions);
        fs::write(&journal_dir, b"not-a-directory")?;

        let result = scan([sessions]);

        assert!(result.entries.is_empty());
        assert_eq!(result.unreadable_dirs.len(), 1);
        assert_eq!(result.unreadable_dirs[0].0, journal_dir);
        Ok(())
    }

    #[test]
    fn record_and_scan_keep_the_existing_path_and_flat_schema() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source/sessions.json");
        let target = temp.path().join("target/sessions.json");
        fs::create_dir_all(source.parent().unwrap())?;
        fs::create_dir_all(target.parent().unwrap())?;
        let path = record(&entry(&source, &target), &source)?;
        assert_eq!(path.parent(), Some(journal_dir_for(&source).as_path()));
        let value: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        assert!(value.get("intent").is_none());

        let result = scan([source]);

        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].0, path);
        assert_eq!(result.entries[0].1.as_ref().unwrap().ids[0], "session-id");
        Ok(())
    }

    #[test]
    fn scan_reads_a_flat_record_from_the_existing_move_journal_directory() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source/sessions.json");
        let target = temp.path().join("target/sessions.json");
        fs::create_dir_all(source.parent().unwrap())?;
        fs::create_dir_all(target.parent().unwrap())?;
        let journal_dir = journal_dir_for(&source);
        fs::create_dir_all(&journal_dir)?;
        let path = journal_dir.join("move-1-1.json");
        fs::write(&path, serde_json::to_vec(&entry(&source, &target))?)?;

        let result = scan([source]);

        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].0, path);
        assert_eq!(result.entries[0].1.as_ref().unwrap().ids, ["session-id"]);
        Ok(())
    }

    #[test]
    fn scan_ignores_unpublished_atomic_write_temporary_files() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let sessions = temp.path().join("profile/sessions.json");
        fs::create_dir_all(sessions.parent().unwrap())?;
        let journal_dir = journal_dir_for(&sessions);
        fs::create_dir_all(&journal_dir)?;
        let _current = tempfile::Builder::new()
            .prefix(crate::session::storage::ATOMIC_WRITE_TEMP_PREFIX)
            .tempfile_in(&journal_dir)?;
        let _legacy = tempfile::NamedTempFile::new_in(&journal_dir)?;

        let result = scan([sessions]);

        assert!(result.unreadable_dirs.is_empty());
        assert!(result.entries.is_empty());
        Ok(())
    }

    #[test]
    fn scan_rejects_unsupported_move_record_versions() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source/sessions.json");
        let target = temp.path().join("target/sessions.json");
        fs::create_dir_all(source.parent().unwrap())?;
        fs::create_dir_all(target.parent().unwrap())?;
        let journal_dir = journal_dir_for(&source);
        fs::create_dir_all(&journal_dir)?;
        let path = journal_dir.join("move-unsupported.json");
        let mut unsupported = entry(&source, &target);
        unsupported.version = MOVE_JOURNAL_VERSION + 1;
        fs::write(&path, serde_json::to_vec(&unsupported)?)?;

        let result = scan([source]);

        let error = result.entries[0].1.as_ref().unwrap_err();
        assert!(error.contains("move journal version 2 is not supported"));
        Ok(())
    }
}
