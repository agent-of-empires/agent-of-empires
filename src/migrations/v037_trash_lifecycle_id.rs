//! Assign durable identities to pre-existing trash lifecycles.

use super::sessions_file;
use anyhow::Result;
use std::{fs, path::Path};
use tracing::info;

pub fn run() -> Result<()> {
    run_in(&crate::session::get_app_dir()?)
}

fn run_in(app_dir: &Path) -> Result<()> {
    for path in sessions_file::session_files(app_dir)? {
        if !path.exists() {
            continue;
        }
        let _lock = crate::session::acquire_storage_flock(
            path.parent().expect("session file has a parent"),
            crate::session::STORAGE_LOCK_FILENAME,
        )?;
        let changed = sessions_file::heal_rows(&path, &fs::read_to_string(&path)?, |row| {
            if row.get("trashed_at").is_some_and(|at| !at.is_null())
                && !row
                    .get("trash_lifecycle_id")
                    .is_some_and(|id| !id.is_null())
            {
                row.insert(
                    "trash_lifecycle_id".into(),
                    serde_json::Value::String(uuid::Uuid::new_v4().to_string()),
                );
                true
            } else {
                false
            }
        })?;
        if changed > 0 {
            info!(target: "migrations.v037", changed, "assigned trash lifecycle identities");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn migrates_legacy_and_profile_trash_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        for parent in [
            dir.path().to_path_buf(),
            dir.path().join("profiles/default"),
        ] {
            fs::create_dir_all(&parent).unwrap();
            let path = parent.join("sessions.json");
            fs::write(
                &path,
                json!([
                    {"id":"old", "trashed_at":"2026-10-11T00:00:00Z"},
                    {"id":"live", "trashed_at":null},
                    {"id":"new", "trashed_at":"2026-10-11T00:00:00Z", "trash_lifecycle_id":"existing"}
                ]).to_string(),
            ).unwrap();
        }
        run_in(dir.path()).unwrap();
        for path in sessions_file::session_files(dir.path()).unwrap() {
            let first = fs::read_to_string(&path).unwrap();
            let rows: Value = serde_json::from_str(&first).unwrap();
            uuid::Uuid::parse_str(rows[0]["trash_lifecycle_id"].as_str().unwrap()).unwrap();
            assert!(rows[1].get("trash_lifecycle_id").is_none());
            assert_eq!(rows[2]["trash_lifecycle_id"], "existing");
            run_in(dir.path()).unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), first);
        }
    }
}
