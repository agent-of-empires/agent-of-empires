//! Reconcile trash identity for installs that already advanced past upstream v037.

use super::sessions_file;
use anyhow::Result;
use serde_json::Value;
use std::fs;
use tracing::info;

pub fn run() -> Result<()> {
    // Keep the published migration unchanged and replay its idempotent work for
    // the PR's schema-42 installations, which never ran upstream's schema 37.
    super::v037_trash_lifecycle_id::run()?;
    for path in sessions_file::session_files(&crate::session::get_app_dir()?)? {
        if !path.exists() {
            continue;
        }
        let _lock = crate::session::acquire_storage_flock(
            path.parent().expect("session file has a parent"),
            crate::session::STORAGE_LOCK_FILENAME,
        )?;
        let changed = sessions_file::heal_rows(&path, &fs::read_to_string(&path)?, |row| {
            let Some(failed) = row.get("failed_purge").filter(|failed| !failed.is_null()) else {
                return false;
            };
            let identity = row.get("trash_lifecycle_id").and_then(Value::as_str);
            let matches = row
                .get("trashed_at")
                .is_some_and(|at| !at.is_null() && failed.get("trashed_at") == Some(at))
                && identity.is_some()
                && failed.get("forced").is_some_and(Value::is_boolean)
                && failed.get("lifecycle_generation").and_then(Value::as_u64)
                    == Some(
                        row.get("lifecycle_generation")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                    )
                && failed
                    .get("trash_lifecycle_id")
                    .is_none_or(|id| id.is_null() || id.as_str() == identity);
            if !matches {
                row.remove("failed_purge");
                return true;
            }
            if failed
                .get("trash_lifecycle_id")
                .is_some_and(|id| !id.is_null())
            {
                return false;
            }
            let identity = identity
                .expect("matching failed purge has a trash identity")
                .to_owned();
            row.get_mut("failed_purge")
                .and_then(Value::as_object_mut)
                .expect("matching failed purge is an object")
                .insert("trash_lifecycle_id".into(), Value::String(identity));
            true
        })?;
        if changed > 0 {
            info!(target: "migrations.v043", changed, "reconciled trash identity for failed purges");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    #[serial_test::serial]
    fn upgrades_both_schema_histories_and_reconciles_failed_purges_idempotently() {
        for version in [36, 37, 42] {
            let dir = tempfile::tempdir().unwrap();
            let _guard = crate::session::test_support::isolate_app_dir_at(dir.path());
            let app = crate::session::get_app_dir().unwrap();
            fs::write(app.join(".schema_version"), version.to_string()).unwrap();
            let existing = uuid::Uuid::new_v4().to_string();
            let stamp = "2026-10-11T00:00:00Z";
            let failure = json!({"trashed_at": stamp, "lifecycle_generation": 7, "forced": true});
            for parent in [app.clone(), app.join("profiles/default")] {
                fs::create_dir_all(&parent).unwrap();
                fs::write(
                    parent.join("sessions.json"),
                    json!([
                        {"id":"old", "trashed_at":stamp, "lifecycle_generation":7, "failed_purge":failure},
                        {"id":"existing", "trashed_at":stamp, "trash_lifecycle_id":existing, "lifecycle_generation":7, "failed_purge":failure},
                        {"id":"bound", "trashed_at":stamp, "trash_lifecycle_id":existing, "lifecycle_generation":7,
                         "failed_purge":{"trashed_at":stamp, "trash_lifecycle_id":existing, "lifecycle_generation":7, "forced":false}},
                        {"id":"stale-generation", "trashed_at":stamp, "lifecycle_generation":8, "failed_purge":failure},
                        {"id":"stale-identity", "trashed_at":stamp, "trash_lifecycle_id":existing, "lifecycle_generation":7,
                         "failed_purge":{"trashed_at":stamp, "trash_lifecycle_id":"previous", "lifecycle_generation":7, "forced":true}},
                        {"id":"restored", "trashed_at":null, "lifecycle_generation":7, "failed_purge":failure},
                        {"id":"live"}
                    ]).to_string(),
                ).unwrap();
            }

            crate::migrations::run_migrations().unwrap();
            assert_eq!(
                fs::read_to_string(app.join(".schema_version"))
                    .unwrap()
                    .trim()
                    .parse::<u32>()
                    .unwrap(),
                super::super::CURRENT_VERSION
            );
            for path in sessions_file::session_files(&app).unwrap() {
                let first = fs::read_to_string(&path).unwrap();
                let rows: Value = serde_json::from_str(&first).unwrap();
                uuid::Uuid::parse_str(rows[0]["trash_lifecycle_id"].as_str().unwrap()).unwrap();
                assert_eq!(
                    rows[0]["failed_purge"]["trash_lifecycle_id"],
                    rows[0]["trash_lifecycle_id"]
                );
                assert_eq!(rows[0]["failed_purge"]["forced"], true);
                for index in [1, 2] {
                    assert_eq!(rows[index]["trash_lifecycle_id"], existing);
                    assert_eq!(rows[index]["failed_purge"]["trash_lifecycle_id"], existing);
                }
                assert_eq!(rows[2]["failed_purge"]["forced"], false);
                for index in [3, 4, 5] {
                    assert!(rows[index].get("failed_purge").is_none());
                }
                assert!(rows[5].get("trash_lifecycle_id").is_none());
                assert!(rows[6].get("trash_lifecycle_id").is_none());
                // Exercise the migration itself twice, not only the version gate.
                run().unwrap();
                assert_eq!(fs::read_to_string(&path).unwrap(), first);
                crate::migrations::run_migrations().unwrap();
                assert_eq!(fs::read_to_string(&path).unwrap(), first);
            }
        }
    }
}
