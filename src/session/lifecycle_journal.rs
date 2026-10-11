//! Durable intent records for session lifecycle work that crosses process boundaries.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::storage::{atomic_write_verified, sync_parent_directory};
use super::{Instance, Status};

/// Bump when the record shape changes. Unsupported records stay on disk so a newer
/// binary cannot silently discard recovery evidence written by an older one.
pub(crate) const LIFECYCLE_JOURNAL_VERSION: u32 = 3;
const EARLIEST_SUPPORTED_LIFECYCLE_JOURNAL_VERSION: u32 = 1;

const JOURNAL_DIR_NAME: &str = ".lifecycle-journal";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LifecyclePhase {
    Reserved,
    HooksStarted,
    HooksComplete,
    TeardownStarted,
    TeardownComplete,
    RowRemoved,
    Kept,
    Abandoned,
}

impl LifecyclePhase {
    pub(crate) fn hooks_are_complete(self) -> bool {
        matches!(
            self,
            Self::HooksComplete
                | Self::TeardownStarted
                | Self::TeardownComplete
                | Self::RowRemoved
                | Self::Kept
        )
    }

    pub(crate) fn teardown_started(self) -> bool {
        matches!(
            self,
            Self::TeardownStarted | Self::TeardownComplete | Self::RowRemoved | Self::Kept
        )
    }

    pub(crate) fn teardown_complete(self) -> bool {
        matches!(self, Self::TeardownComplete | Self::RowRemoved | Self::Kept)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "intent", content = "payload", rename_all = "snake_case")]
pub(crate) enum LifecycleJournalRecord {
    Deleting(Box<LifecycleJournalEntry>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LifecycleJournalEntry {
    pub(crate) version: u32,
    pub(crate) phase: LifecyclePhase,
    pub(crate) session_id: String,
    pub(crate) source_profile: String,
    pub(crate) sessions_path: PathBuf,
    pub(crate) instance: Instance,
    pub(crate) status_before: Status,
    pub(crate) delete_worktree: bool,
    pub(crate) delete_branch: bool,
    pub(crate) delete_sandbox: bool,
    pub(crate) force_delete: bool,
    pub(crate) detach_hooks: bool,
    pub(crate) keep_scratch: bool,
    #[serde(default)]
    pub(crate) purge_acp_transcript: bool,
    pub(crate) generation: u64,
    pub(crate) created_at_epoch_ms: u64,
    pub(crate) kept_resources: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct LifecycleDeletionOptions {
    pub(crate) generation: u64,
    pub(crate) delete_worktree: bool,
    pub(crate) delete_branch: bool,
    pub(crate) delete_sandbox: bool,
    pub(crate) force_delete: bool,
    pub(crate) detach_hooks: bool,
    pub(crate) keep_scratch: bool,
    pub(crate) purge_acp_transcript: bool,
}

impl LifecycleJournalEntry {
    pub(crate) fn is_current(&self) -> bool {
        (EARLIEST_SUPPORTED_LIFECYCLE_JOURNAL_VERSION..=LIFECYCLE_JOURNAL_VERSION)
            .contains(&self.version)
    }

    pub(crate) fn deletion(
        instance: Instance,
        status_before: Status,
        sessions_path: PathBuf,
        options: LifecycleDeletionOptions,
    ) -> Self {
        let session_id = instance.id.clone();
        let source_profile = instance.source_profile.clone();
        Self {
            version: LIFECYCLE_JOURNAL_VERSION,
            phase: LifecyclePhase::Reserved,
            session_id,
            source_profile,
            sessions_path,
            instance,
            status_before,
            delete_worktree: options.delete_worktree,
            delete_branch: options.delete_branch,
            delete_sandbox: options.delete_sandbox,
            force_delete: options.force_delete,
            detach_hooks: options.detach_hooks,
            keep_scratch: options.keep_scratch,
            purge_acp_transcript: options.purge_acp_transcript,
            generation: options.generation,
            created_at_epoch_ms: now_epoch_ms(),
            kept_resources: Vec::new(),
        }
    }

    pub(crate) fn with_phase(&self, phase: LifecyclePhase) -> Self {
        let mut next = self.clone();
        next.version = LIFECYCLE_JOURNAL_VERSION;
        next.phase = phase;
        next
    }

    pub(crate) fn with_kept_resources(&self, resources: Vec<String>) -> Self {
        let mut next = self.clone();
        next.version = LIFECYCLE_JOURNAL_VERSION;
        next.kept_resources = resources;
        next
    }

    pub(crate) fn with_generation(&self, generation: u64) -> Self {
        let mut next = self.clone();
        next.version = LIFECYCLE_JOURNAL_VERSION;
        next.generation = generation;
        next.instance.lifecycle_generation = generation;
        if let Some(reservation) = next.instance.lifecycle_reservation.as_mut() {
            reservation.generation = generation;
            reservation.at = chrono::Utc::now();
        }
        next
    }
}

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn journal_dir_for(sessions_path: &Path) -> PathBuf {
    sessions_path
        .parent()
        .unwrap_or(sessions_path)
        .join(JOURNAL_DIR_NAME)
}

pub(crate) fn deletion_exists(
    sessions_path: &Path,
    session_id: &str,
    generation: u64,
) -> Result<bool> {
    let session_id_hex = session_id
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let path =
        journal_dir_for(sessions_path).join(format!("delete-{generation}-{session_id_hex}.json"));
    match read_deletion(&path)? {
        Some(entry) => Ok(entry.session_id == session_id && entry.generation == generation),
        None => Ok(false),
    }
}

pub(crate) fn record(entry: &LifecycleJournalEntry) -> Result<PathBuf> {
    let dir = journal_dir_for(&entry.sessions_path);
    let session_id_hex = entry
        .session_id
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let path = dir.join(format!("delete-{}-{session_id_hex}.json", entry.generation));
    record_at_with_sync(
        &dir,
        &path,
        &LifecycleJournalRecord::Deleting(Box::new(entry.clone())),
        sync_parent_directory,
    )
}

fn record_at_with_sync<S>(
    dir: &Path,
    path: &Path,
    record: &LifecycleJournalRecord,
    mut sync: S,
) -> Result<PathBuf>
where
    S: FnMut(&Path) -> Result<()>,
{
    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    sync(dir)
        .with_context(|| format!("journal directory {} was not made durable", dir.display()))?;
    let bytes = serde_json::to_vec_pretty(record)?;
    atomic_write_verified(path, &bytes)
        .with_context(|| format!("failed to write lifecycle journal {}", path.display()))?;
    sync(path)
        .with_context(|| format!("lifecycle journal {} was not made durable", path.display()))?;
    Ok(path.to_path_buf())
}

pub(crate) fn update(path: &Path, entry: &LifecycleJournalEntry) -> Result<()> {
    let bytes =
        serde_json::to_vec_pretty(&LifecycleJournalRecord::Deleting(Box::new(entry.clone())))?;
    atomic_write_verified(path, &bytes)
        .with_context(|| format!("failed to update lifecycle journal {}", path.display()))?;
    sync_parent_directory(path)
        .with_context(|| format!("lifecycle journal {} was not made durable", path.display()))
}

pub(crate) fn consume(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to remove lifecycle journal {}", path.display()))
        }
    }
    sync_parent_directory(path)
        .with_context(|| format!("removal of {} was not made durable", path.display()))
}

pub(crate) struct ScanResult {
    pub(crate) entries: Vec<(PathBuf, std::result::Result<LifecycleJournalEntry, String>)>,
    pub(crate) unreadable_dirs: Vec<(PathBuf, String)>,
}

pub(crate) fn scan(sessions_paths: impl IntoIterator<Item = PathBuf>) -> ScanResult {
    let mut result = ScanResult {
        entries: Vec::new(),
        unreadable_dirs: Vec::new(),
    };
    let mut dirs: Vec<PathBuf> = sessions_paths
        .into_iter()
        .map(|path| journal_dir_for(&path))
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
        let mut paths: Vec<PathBuf> = read_dir
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect();
        paths.sort();
        for path in paths {
            let parsed = match read_record(&path) {
                Ok(LifecycleJournalRecord::Deleting(entry)) if entry.is_current() => {
                    Some(Ok(*entry))
                }
                Ok(LifecycleJournalRecord::Deleting(entry)) => Some(Err(format!(
                        "lifecycle journal version {} is not supported (current: {LIFECYCLE_JOURNAL_VERSION})",
                        entry.version
                    ))),
                Err(error) => Some(Err(format!("{error:#}"))),
            };
            if let Some(parsed) = parsed {
                result.entries.push((path, parsed));
            }
        }
    }
    result
}

fn read_record(path: &Path) -> Result<LifecycleJournalRecord> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes)
        .or_else(|record_error| {
            serde_json::from_slice::<LifecycleJournalEntry>(&bytes)
                .map(|entry| LifecycleJournalRecord::Deleting(Box::new(entry)))
                .map_err(|_| record_error)
        })
        .context("malformed lifecycle journal entry")
}

pub(crate) fn read_deletion(path: &Path) -> Result<Option<LifecycleJournalEntry>> {
    match read_record(path)? {
        LifecycleJournalRecord::Deleting(entry) if entry.is_current() => Ok(Some(*entry)),
        LifecycleJournalRecord::Deleting(entry) => anyhow::bail!(
            "lifecycle journal version {} is not supported (current: {})",
            entry.version,
            LIFECYCLE_JOURNAL_VERSION
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletion_entry_roundtrips_and_phase_updates() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("profile/sessions.json");
        std::fs::create_dir_all(sessions.parent().unwrap()).unwrap();
        let mut instance = Instance::new("journal-session", "/tmp/worktree");
        instance.id = "journal-session".to_string();
        instance.source_profile = "profile".to_string();
        let entry = LifecycleJournalEntry::deletion(
            instance,
            Status::Idle,
            sessions,
            LifecycleDeletionOptions {
                generation: 7,
                delete_worktree: true,
                delete_branch: true,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
                purge_acp_transcript: false,
            },
        );
        let path = record(&entry).unwrap();
        let initial_scan = scan([path
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("sessions.json")]);
        let (found_path, found) = initial_scan.entries.into_iter().next().unwrap();
        assert_eq!(found_path, path);
        let found = found.unwrap();
        assert_eq!(found.session_id, "journal-session");
        assert_eq!(found.phase, LifecyclePhase::Reserved);

        let updated = found.with_phase(LifecyclePhase::Kept);
        update(&path, &updated).unwrap();
        let updated_scan = scan([path
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("sessions.json")]);
        assert_eq!(
            updated_scan.entries[0].1.as_ref().unwrap().phase,
            LifecyclePhase::Kept
        );
        consume(&path).unwrap();
        assert!(scan([path
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("sessions.json")])
        .entries
        .is_empty());
    }

    #[test]
    fn version_one_deletion_records_default_to_no_transcript_cleanup() {
        let mut instance = Instance::new("journal-session", "/tmp/worktree");
        instance.id = "journal-session".to_string();
        instance.source_profile = "profile".to_string();
        let mut entry = LifecycleJournalEntry::deletion(
            instance,
            Status::Idle,
            PathBuf::from("/tmp/profile/sessions.json"),
            LifecycleDeletionOptions {
                generation: 1,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: false,
                keep_scratch: false,
                purge_acp_transcript: false,
            },
        );
        entry.version = 1;
        let mut record =
            serde_json::to_value(LifecycleJournalRecord::Deleting(Box::new(entry))).unwrap();
        record["payload"]
            .as_object_mut()
            .unwrap()
            .remove("purge_acp_transcript");

        let parsed: LifecycleJournalRecord = serde_json::from_value(record).unwrap();
        let LifecycleJournalRecord::Deleting(entry) = parsed;
        assert!(entry.is_current());
        assert!(!entry.purge_acp_transcript);
    }
}
