use std::path::Path;

use anyhow::{Context, Result};

use crate::session::{purge_owners, AnchoredDir};

pub fn run() -> Result<()> {
    migrate(&crate::session::get_app_dir()?, false)?;
    tracing::info!("Migrated pending purge runner ownership");
    Ok(())
}

pub(super) fn migrate(root: &Path, initialize_missing: bool) -> Result<()> {
    let file = AnchoredDir::open(root)?.bind_file(purge_owners::FILE_NAME.as_ref())?;
    let (lock, path) = file.open_sidecar()?;
    let guard = crate::session::acquire_open_storage_flock(lock, &path)?;
    let Some(content) = file.read()? else {
        anyhow::ensure!(
            initialize_missing,
            "Pending purge ownership journal is missing"
        );
        drop(guard);
        return purge_owners::initialize(root);
    };
    let mut journal: serde_json::Value = serde_json::from_str(&content)?;
    match journal.get("version").and_then(serde_json::Value::as_u64) {
        Some(2) => return purge_owners::validate_serialized(&content),
        Some(1) => {}
        _ => anyhow::bail!("Unsupported pending purge ownership version"),
    }
    let owners = journal
        .get_mut("owners")
        .and_then(serde_json::Value::as_array_mut)
        .context("Invalid pending purge owners")?;
    for owner in owners {
        let owner = owner
            .as_object_mut()
            .context("Invalid pending purge owner")?;
        anyhow::ensure!(
            !owner.contains_key("runner"),
            "Unexpected runner evidence in legacy journal"
        );
        owner.insert("runner".into(), serde_json::json!({"state": "uncaptured"}));
    }
    journal["version"] = 2.into();
    let content = serde_json::to_string(&journal)?;
    purge_owners::validate_serialized(&content)?;
    file.replace(content.as_bytes())?;
    file.sync_parent()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_migration_preserves_ownership_and_refuses_missing_evidence() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join(purge_owners::FILE_NAME);
        let resource = root.path().join("owned");
        let legacy = serde_json::json!({
            "version": 1,
            "owners": [{
                "token": "00000000-0000-4000-8000-000000000001",
                "session_id": "0123456789abcdef", "profile": "source", "generation": 7,
                "protection": {
                    "paths": [{"lexical": resource, "traversal": null, "resolved": null}],
                    "branches": [[0, "work"]]
                }
            }]
        });
        std::fs::write(&path, serde_json::to_vec(&legacy)?)?;
        let storage =
            crate::session::Storage::new_for_test_path("source", root.path().join("sessions.json"));
        for _ in 0..2 {
            migrate(root.path(), false)?;
            let owners = purge_owners::protection(&storage, None)?;
            assert!(owners.iter().any(|owner| owner.references_path(&resource)));
            assert!(owners
                .iter()
                .any(|owner| owner.references_branch(&resource, "work")));
        }
        std::fs::write(&path, "{")?;
        assert!(migrate(root.path(), false).is_err());
        assert_eq!(std::fs::read_to_string(&path)?, "{");
        std::fs::remove_file(&path)?;
        assert!(migrate(root.path(), false).is_err());
        assert!(
            !path.exists(),
            "upgrade cannot reconstruct lost ownership as empty"
        );
        Ok(())
    }
}
