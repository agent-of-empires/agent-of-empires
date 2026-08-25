//! Durable journal for cross-profile session moves (#3459).
//!
//! `Storage::move_instances_to_inner` writes target rows before removing
//! source rows, so a crash in between leaves the same session id in two
//! profiles. This module is the evidence that arbitrates such states: one
//! versioned JSON file per attempted move, stored under the source profile's
//! directory (`.move-journal/` next to `sessions.json`), written (and fsynced)
//! before the first mutation and deleted only after the move completed or a
//! recovery pass consumed it.
//!
//! The journal lives with the source store instead of a global app-dir folder
//! so its lifetime is bounded by exactly the stores it arbitrates between,
//! and tests driving `Storage` at arbitrary paths stay hermetic without
//! process-global coordination.
//!
//! Winner policy: a valid, current-version journal entry is proof that a move
//! was in flight between exactly the two recorded stores. Because the
//! transaction publishes the target before touching the source, whichever
//! store currently holds the id is the winner; the other copy loses. No
//! iteration order is ever consulted. An entry that cannot be parsed or
//! whose version differs is treated as insufficient evidence: the duplicate
//! is surfaced for manual resolution instead of arbitrated.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::storage::{atomic_write_verified, sync_parent_directory};

/// Bump when the entry shape changes. Entries written by an older version
/// carry no arbitration authority (see [`MoveJournalEntry::is_current`]).
pub(crate) const MOVE_JOURNAL_VERSION: u32 = 1;

const JOURNAL_DIR_NAME: &str = ".move-journal";

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

/// `.move-journal/` inside the directory holding `sessions_path`.
fn journal_dir_for(sessions_path: &Path) -> PathBuf {
    sessions_path
        .parent()
        .unwrap_or(sessions_path)
        .join(JOURNAL_DIR_NAME)
}

/// Persist one entry next to the source profile's `sessions.json` and return
/// its path. The write is atomic and fsynced (file content plus parent
/// directory), so once [`record`] returns Ok the entry survives a power loss.
pub(crate) fn record(entry: &MoveJournalEntry, source_sessions_path: &Path) -> Result<PathBuf> {
    let dir = journal_dir_for(source_sessions_path);
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let path = dir.join(format!("move-{}-{}.json", nanos, std::process::id()));
    let bytes = serde_json::to_vec_pretty(entry)?;
    atomic_write_verified(&path, &bytes)
        .with_context(|| format!("failed to write move journal {}", path.display()))?;
    sync_parent_directory(&path)
        .with_context(|| format!("move journal {} was not made durable", path.display()))?;
    Ok(path)
}

/// Delete one consumed entry and sync its parent directory so the removal
/// itself is durable. Idempotent: a missing file is already consumed.
pub(crate) fn consume(path: &Path) -> Result<()> {
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

/// Every entry under each given profile directory paired with its parse
/// outcome. `Err` covers unreadable files, malformed JSON, and wrong-version
/// entries: all of them are evidence that *something* happened but not enough
/// to arbitrate automatically.
pub(crate) fn scan(
    sessions_paths: impl IntoIterator<Item = PathBuf>,
) -> Vec<(PathBuf, std::result::Result<MoveJournalEntry, String>)> {
    let mut out = Vec::new();
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
                out.push((
                    dir.clone(),
                    Err(format!("failed to list {}: {error}", dir.display())),
                ));
                continue;
            }
        };
        let mut paths: Vec<PathBuf> = read_dir
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        paths.sort();
        for path in paths {
            let parsed = fs::read(&path)
                .context("failed to read move journal entry")
                .and_then(|bytes| {
                    serde_json::from_slice::<MoveJournalEntry>(&bytes)
                        .context("malformed move journal entry")
                })
                .map_err(|error| format!("{error:#}"))
                .and_then(|entry| {
                    if entry.is_current() {
                        Ok(entry)
                    } else {
                        Err(format!(
                            "move journal version {} is not supported (current: {MOVE_JOURNAL_VERSION})",
                            entry.version
                        ))
                    }
                });
            out.push((path, parsed));
        }
    }
    out
}
