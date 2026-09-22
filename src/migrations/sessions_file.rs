//! Shared `sessions.json` enumeration and row healing for migrations.

use anyhow::Result;
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::debug;

/// Every profile's `sessions.json`, then the legacy top-level one from the
/// pre-profiles layout.
pub(super) fn session_files(app_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let profiles = app_dir.join("profiles");
    if profiles.exists() {
        for entry in fs::read_dir(&profiles)? {
            let path = entry?.path();
            if path.is_dir() {
                paths.push(path.join("sessions.json"));
            }
        }
    }
    paths.push(app_dir.join("sessions.json"));
    Ok(paths)
}

/// Apply `heal` to every row of a `sessions.json` document, writing the file
/// back when any row reports a change, and answering how many did. A document
/// that does not parse is skipped: these heals are best-effort and an
/// unreadable file must not abort boot or spam every launch.
pub(super) fn heal_rows(
    path: &Path,
    content: &str,
    mut heal: impl FnMut(&mut Map<String, Value>) -> bool,
) -> Result<usize> {
    let mut document: Value = match serde_json::from_str(content) {
        Ok(document) => document,
        Err(e) => {
            debug!("failed to parse {}: {e}, skipping", path.display());
            return Ok(0);
        }
    };
    let mut healed = 0usize;
    for row in document.as_array_mut().into_iter().flatten() {
        if let Some(row) = row.as_object_mut() {
            if heal(row) {
                healed += 1;
            }
        }
    }
    if healed > 0 {
        crate::session::atomic_write(path, serde_json::to_string_pretty(&document)?.as_bytes())?;
    }
    Ok(healed)
}

/// Whether a row is archived, i.e. carries a non-null `archived_at`.
pub(super) fn is_archived(row: &Map<String, Value>) -> bool {
    row.get("archived_at").is_some_and(|v| !v.is_null())
}

/// A row's persisted `status`.
pub(super) fn status(row: &Map<String, Value>) -> Option<&str> {
    row.get("status").and_then(|v| v.as_str())
}

/// Put a row back at `idle`.
pub(super) fn settle_to_idle(row: &mut Map<String, Value>) {
    row.insert("status".to_string(), Value::String("idle".to_string()));
}
