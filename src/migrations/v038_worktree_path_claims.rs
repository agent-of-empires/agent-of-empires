//! Legacy Attach leases have no recoverable candidate-path inventory.

use anyhow::{Context, Result};
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
        let mut document = crate::session::raw_document::RawDocument::parse(&content)
            .with_context(|| format!("parsing {}", path.display()))?;
        if migrate_document(&mut document)
            .with_context(|| format!("migrating {}", path.display()))?
        {
            crate::session::backup_before_migration(&path)?;
            crate::session::atomic_write(&path, &serde_json::to_vec_pretty(&document.rows)?)?;
            crate::session::sync_parent_directory(&path)?;
        }
    }
    Ok(())
}

fn migrate_document(document: &mut crate::session::raw_document::RawDocument) -> Result<bool> {
    use crate::session::raw_document::{patch, Emission, RawObject};
    let mut changed = false;
    for row in &mut document.rows {
        let Ok(fields) = RawObject::parse(row) else {
            continue;
        };
        let Ok(Some(lease)) = fields.unique("lifecycle_reservation") else {
            continue;
        };
        let Ok(lease) = RawObject::parse(lease) else {
            continue;
        };
        if !matches!(lease.unique("path_claims"), Ok(None)) {
            continue;
        }
        let Ok(Some(operation)) = lease.unique("op") else {
            continue;
        };
        let Ok(operation) = serde_json::from_str::<String>(operation.get()) else {
            continue;
        };
        let state = match operation.as_str() {
            "attach" | "create" => "unknown",
            "launch" | "capture" | "stop" | "purge" | "restore" | "trash" => "none",
            _ => continue,
        };
        let before = serde_json::json!({"lifecycle_reservation": {"op": operation}});
        let after = serde_json::json!({"lifecycle_reservation": {"op": operation, "path_claims": {"state": state}}});
        if let Emission::Changed(updated) = patch(row, &before, &after)? {
            *row = updated;
            changed = true;
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::raw_document::RawDocument;
    use serde::Deserialize;
    use serde_json::Value;

    #[test]
    fn migration_retains_literal_extensions_and_ambiguous_lease_fields() {
        let extension = r#"{"same":1,"same":2,"big":1234567890123456789012345678901234567890,"float":1e400,"escaped":"\u0061"}"#;
        let ambiguous = r#"{"id":"duplicated","lifecycle_reservation":{"op":"launch","op":"attach","generation":0},"extra":true}"#;
        let first = format!(
            r#"{{"id":"duplicated","title":42,"lifecycle_reservation":{{"op":"attach","generation":"broken","at":null,"extension":{extension}}},"extension":{extension}}}"#
        );
        let mut document = RawDocument::parse(&format!("[{first},{ambiguous}]")).unwrap();
        assert!(migrate_document(&mut document).unwrap());
        use crate::session::raw_document::RawObject;
        let row = RawObject::parse(&document.rows[0]).unwrap();
        assert_eq!(row.unique("extension").unwrap().unwrap().get(), extension);
        let lease =
            RawObject::parse(row.unique("lifecycle_reservation").unwrap().unwrap()).unwrap();
        assert_eq!(lease.unique("extension").unwrap().unwrap().get(), extension);
        assert_eq!(
            lease.unique("path_claims").unwrap().unwrap().get(),
            r#"{"state":"unknown"}"#
        );
        assert_eq!(document.rows[1].get(), ambiguous);
        let before = serde_json::to_vec(&document.rows).unwrap();
        assert!(!migrate_document(&mut document).unwrap());
        assert_eq!(serde_json::to_vec(&document.rows).unwrap(), before);
    }

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
        let mut raw = RawDocument::parse(&serde_json::to_string(&rows).unwrap()).unwrap();
        assert!(migrate_document(&mut raw).unwrap());
        let document: Value =
            serde_json::from_slice(&serde_json::to_vec(&raw.rows).unwrap()).unwrap();
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
        let original = serde_json::to_vec(&raw.rows).unwrap();
        assert!(!migrate_document(&mut raw).unwrap());
        assert_eq!(serde_json::to_vec(&raw.rows).unwrap(), original);
        let migrated = crate::session::Instance::deserialize(&document[0]).unwrap();
        assert!(migrated.has_active_lifecycle_reservation("2020-01-01T00:00:00Z".parse().unwrap()));
        let retained = Value::Array(vec![
            document[0].clone(),
            document[0].clone(),
            Value::Null,
            serde_json::json!({"lifecycle_reservation": "broken"}),
            serde_json::json!({"lifecycle_reservation": {"op": "unsupported"}}),
            serde_json::json!({"lifecycle_reservation": {"op": "attach", "path_claims": {"state": "broken"}}}),
        ]);
        let mut retained = RawDocument::parse(&serde_json::to_string(&retained).unwrap()).unwrap();
        let before = serde_json::to_vec(&retained.rows).unwrap();
        assert!(!migrate_document(&mut retained).unwrap());
        assert_eq!(serde_json::to_vec(&retained.rows).unwrap(), before);
        assert!(RawDocument::parse("{\"instances\": []}").is_err());
    }
}
