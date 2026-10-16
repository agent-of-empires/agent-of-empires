//! Reconcile the schema-38 lifecycle branch with main's worker ownership model.

use std::fs;

use anyhow::Result;
use serde_json::Value;

pub fn run() -> Result<()> {
    // Schema 38 named two different migrations on the PR branch and on main.
    // Replaying main's migration is safe and covers installs that recorded the PR version.
    super::v038_prime_native_runtime::run()?;

    let workers = crate::process::worker_registry::workers_dir()?;
    if !workers.try_exists()? {
        return Ok(());
    }
    for entry in fs::read_dir(&workers)? {
        let entry = entry?;
        let path = entry.path();
        let Some(extension) = path.extension().and_then(|value| value.to_str()) else {
            continue;
        };
        if !matches!(extension, "json" | "orphan-stop" | "purge-fence") {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let suffix = format!(".{extension}");
        let Some(session_id) = file_name.strip_suffix(&suffix) else {
            continue;
        };
        crate::process::worker_registry::with_registry_lock(session_id, || migrate_file(&path))?;
    }
    Ok(())
}

fn migrate_file(path: &std::path::Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Ok(());
    }
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) else {
        tracing::warn!(path = %path.display(), "v042: preserving unreadable lifecycle evidence");
        return Ok(());
    };
    let Some(object) = value.as_object_mut() else {
        return Ok(());
    };
    let process_identity = object.remove("process_identity").is_some();
    let runner_process_identity = object.remove("runner_process_identity").is_some();
    let legacy_runner_identity = object
        .get("runner_identity")
        .and_then(|identity| identity.get("start_id"))
        .and_then(Value::as_str)
        .is_some_and(|start_id| start_id.starts_with("audit:"));
    if !process_identity && !runner_process_identity && !legacy_runner_identity {
        return Ok(());
    }
    // Unknown evidence retains the ownership fence instead of mistaking a format change for exit.
    if legacy_runner_identity || !object.contains_key("runner_identity") {
        object.insert("runner_identity".into(), Value::Null);
    }
    crate::session::backup_before_migration(path)?;
    crate::session::atomic_write(path, &serde_json::to_vec_pretty(&value)?)?;
    crate::session::sync_parent_directory(path)?;
    tracing::info!(path = %path.display(), "v042: migrated lifecycle ownership evidence");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn corrected_41_preserves_supported_birth_and_cuts_over_duplicate_authority() {
        let root = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        let path = crate::process::worker_registry::workers_dir()
            .unwrap()
            .join("supported.json");
        let identity =
            serde_json::json!({"boot_id": uuid::Uuid::nil().to_string(), "start_id": "proc:123"});
        fs::write(&path, serde_json::to_vec(&serde_json::json!({
            "pid": 42, "runner_identity": identity, "process_identity": {"start_id": 1}, "opaque": [1,2,3]
        })).unwrap()).unwrap();
        super::super::v041_kernel_process_identity::run().unwrap();
        let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["runner_identity"], identity);
        assert_eq!(value["opaque"], serde_json::json!([1, 2, 3]));
        assert!(value.get("process_identity").is_none());
        let once = fs::read(&path).unwrap();
        run().unwrap();
        assert_eq!(fs::read(&path).unwrap(), once);
    }

    #[test]
    #[serial_test::serial]
    fn legacy_kernel_evidence_becomes_unknown_without_changing_extensions() {
        let root = tempfile::TempDir::with_prefix_in("aoe-migrate-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        let workers = crate::process::worker_registry::workers_dir().unwrap();
        let path = workers.join("legacy.json");
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "pid": 42,
                "process_identity": {"boot_id": uuid::Uuid::nil(), "start_id": 7},
                "runner_identity": null,
                "extension": {"keep": true}
            }))
            .unwrap(),
        )
        .unwrap();
        let audit = workers.join("legacy-audit.json");
        fs::write(
            &audit,
            serde_json::to_vec(&serde_json::json!({
                "pid": 45,
                "runner_identity": {
                    "boot_id": "boot",
                    "start_id": "audit:0000000100000002"
                },
                "extension": "audit"
            }))
            .unwrap(),
        )
        .unwrap();

        let orphan = workers.join("legacy.orphan-stop");
        fs::write(
            &orphan,
            serde_json::to_vec(&serde_json::json!({
                "pid": 43,
                "process_identity": {"boot_id": uuid::Uuid::nil(), "start_id": 8},
                "extension": "orphan"
            }))
            .unwrap(),
        )
        .unwrap();
        let fence = workers.join("legacy.purge-fence");
        fs::write(
            &fence,
            serde_json::to_vec(&serde_json::json!({
                "runner_pid": 44,
                "runner_process_identity": {
                    "boot_id": uuid::Uuid::nil(),
                    "start_id": 9
                },
                "extension": "fence"
            }))
            .unwrap(),
        )
        .unwrap();
        let corrupt = workers.join("corrupt.purge-fence");
        fs::write(&corrupt, b"{").unwrap();
        let outside = root.path().join("outside.json");
        fs::write(
            &outside,
            br#"{"process_identity":{"boot_id":"outside","start_id":1}}"#,
        )
        .unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, workers.join("link.json")).unwrap();

        migrate_file(&workers.join("vanished.json")).unwrap();
        run().unwrap();
        let migrated: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(migrated.get("process_identity").is_none());
        assert!(migrated["runner_identity"].is_null());
        assert_eq!(migrated["extension"], serde_json::json!({"keep": true}));
        let migrated_audit: Value = serde_json::from_slice(&fs::read(&audit).unwrap()).unwrap();
        assert!(migrated_audit["runner_identity"].is_null());
        assert_eq!(migrated_audit["extension"], "audit");
        assert_eq!(fs::read(&corrupt).unwrap(), b"{");
        assert_eq!(
            fs::read(&outside).unwrap(),
            br#"{"process_identity":{"boot_id":"outside","start_id":1}}"#
        );
        assert!(!crate::session::migration_backups(&path).unwrap().is_empty());
        for (evidence, extension) in [(&orphan, "orphan"), (&fence, "fence")] {
            let migrated: Value = serde_json::from_slice(&fs::read(evidence).unwrap()).unwrap();
            assert!(migrated.get("process_identity").is_none());
            assert!(migrated.get("runner_process_identity").is_none());
            assert!(migrated["runner_identity"].is_null());
            assert_eq!(migrated["extension"], extension);
        }
        let once = [
            fs::read(&path).unwrap(),
            fs::read(&orphan).unwrap(),
            fs::read(&fence).unwrap(),
            fs::read(&audit).unwrap(),
        ];
        run().unwrap();
        assert_eq!(fs::read(path).unwrap(), once[0]);
        assert_eq!(fs::read(orphan).unwrap(), once[1]);
        assert_eq!(fs::read(fence).unwrap(), once[2]);
        assert_eq!(fs::read(audit).unwrap(), once[3]);
    }
}
