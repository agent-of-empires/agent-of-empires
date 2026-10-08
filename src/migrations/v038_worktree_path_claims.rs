//! Legacy Attach leases have no recoverable candidate-path inventory.

use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;

pub fn run() -> Result<()> {
    tracing::info!(target: "migrations", "v038: preserving unfinished filesystem intents");
    let _workspace = crate::session::acquire_session_workspace_claim_lock()?;
    let _identity = crate::session::acquire_session_identity_lock()?;
    let _namespace = crate::session::acquire_profile_namespace_lock()?;
    let app = crate::session::get_app_dir()?;
    let mut paths = crate::session::list_profiles_for_worktree_inventory()?
        .into_iter()
        .map(|profile| app.join("profiles").join(profile).join("sessions.json"))
        .collect::<Vec<_>>();
    paths.push(app.join("sessions.json"));
    for path in paths {
        let directory = path.parent().context("sessions file has no parent")?;
        let original = fs::File::open(directory)?;
        let _lock = crate::session::acquire_storage_flock(
            directory,
            crate::session::STORAGE_LOCK_FILENAME,
        )?;
        anyhow::ensure!(
            crate::session::same_filesystem_identity(
                &original.metadata()?,
                &fs::metadata(directory)?
            ),
            "profile changed during filesystem-intent migration"
        );
        let content = match fs::read_to_string(&path) {
            Ok(content) if content.trim().is_empty() => continue,
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::symlink_metadata(&path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    _ => anyhow::bail!("unreadable sessions file {}", path.display()),
                }
            }
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        let mut document: Value = serde_json::from_str(&content)
            .with_context(|| format!("parsing {}", path.display()))?;
        if migrate_document(&mut document)
            .with_context(|| format!("migrating {}", path.display()))?
        {
            crate::session::backup_before_migration(&path)?;
            crate::session::atomic_write(&path, &serde_json::to_vec_pretty(&document)?)?;
            crate::session::sync_parent_directory(&path)?;
        }
    }
    Ok(())
}

fn migrate_document(document: &mut Value) -> Result<bool> {
    let rows = document
        .as_array_mut()
        .context("sessions document is not an array")?;
    let mut changed = false;
    for row in rows {
        let Some(fields) = row.as_object_mut() else {
            continue;
        };
        let Some(lease) = fields
            .get_mut("lifecycle_reservation")
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        if lease.contains_key("path_claims") {
            continue;
        }
        let state = match lease.get("op").and_then(Value::as_str) {
            Some("attach" | "create") => "unknown",
            Some("launch" | "capture" | "stop" | "purge" | "restore" | "trash") => "none",
            _ => continue,
        };
        lease.insert("path_claims".into(), serde_json::json!({"state": state}));
        changed = true;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[test]
    fn migration_preserves_intents_and_unrelated_corrupt_or_duplicate_rows() {
        let mut rows = (0..4)
            .map(|index| {
                serde_json::to_value(crate::session::Instance::new(
                    &format!("row-{index}"),
                    "/tmp/project",
                ))
                .unwrap()
            })
            .collect::<Vec<_>>();
        rows[0]["lifecycle_generation"] = serde_json::json!(100);
        rows[0]["lifecycle_reservation"] =
            serde_json::json!({"op": "attach", "generation": 1, "at": "2000-01-01T00:00:00Z"});
        rows[0]["extra"] = serde_json::json!({"keep": true});
        rows[1]["lifecycle_reservation"] =
            serde_json::json!({"op": "stop", "generation": 2, "at": "2000-01-01T00:00:00Z"});
        rows[2]["lifecycle_reservation"] = serde_json::json!({"op": "attach", "generation": 0, "at": "2000-01-01T00:00:00Z", "path_claims": {"state": "pending", "paths": ["/tmp/future"]}});
        let mut document = Value::Array(rows);
        assert!(migrate_document(&mut document).unwrap());
        assert_eq!(
            document[0]["lifecycle_reservation"]["path_claims"]["state"],
            "unknown"
        );
        assert_eq!(document[0]["extra"], serde_json::json!({"keep": true}));
        assert_eq!(
            document[1]["lifecycle_reservation"]["path_claims"]["state"],
            "none"
        );
        assert_eq!(
            document[2]["lifecycle_reservation"]["path_claims"]["paths"],
            serde_json::json!(["/tmp/future"])
        );
        assert!(!migrate_document(&mut document).unwrap());
        let migrated = crate::session::Instance::deserialize(&document[0]).unwrap();
        assert!(migrated.has_active_lifecycle_reservation("2020-01-01T00:00:00Z".parse().unwrap()));
        let mut retained = Value::Array(vec![
            document[0].clone(),
            document[0].clone(),
            Value::Null,
            serde_json::json!({"lifecycle_reservation": "broken"}),
            serde_json::json!({"lifecycle_reservation": {"op": "unsupported"}}),
            serde_json::json!({"lifecycle_reservation": {"op": "attach", "path_claims": {"state": "broken"}}}),
        ]);
        let before = retained.clone();
        assert!(!migrate_document(&mut retained).unwrap());
        assert_eq!(retained, before);
        assert!(migrate_document(&mut serde_json::json!({"instances": []})).is_err());
    }
}
