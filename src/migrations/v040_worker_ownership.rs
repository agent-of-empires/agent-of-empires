//! Preserve legacy ownership without inventing kernel birth or native epoch authority.

use anyhow::Result;
use serde_json::Value;
use std::{fs, path::Path};

pub fn run() -> Result<()> {
    run_in(&crate::session::get_app_dir()?)
}

fn read_regular(path: &Path) -> Result<Option<Vec<u8>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Ok(None);
    }
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn run_in(app: &Path) -> Result<()> {
    let workers = app.join("acp-workers");
    if !workers.try_exists()? {
        return Ok(());
    }
    for entry in fs::read_dir(&workers)? {
        let path = entry?.path();
        let is_record = path
            .extension()
            .is_some_and(|extension| extension == "json");
        let is_proof = path
            .extension()
            .is_some_and(|extension| extension == "prime-proof");
        if !is_record && !is_proof {
            continue;
        }
        let lock_name = if is_record {
            format!("{}.lock", path.file_stem().unwrap().to_string_lossy())
        } else {
            path.with_extension("prime-lock")
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };
        let _lock = crate::session::acquire_storage_flock(&workers, &lock_name)?;
        let Some(bytes) = read_regular(&path)? else {
            continue;
        };
        let mut value: Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "v040: preserving unreadable ownership evidence");
                continue;
            }
        };
        let Some(object) = value.as_object_mut() else {
            continue;
        };
        let changed = if is_record && !object.contains_key("runner_identity") {
            let identity = if let Ok(record) =
                serde_json::from_slice::<crate::process::worker_registry::WorkerRecord>(&bytes)
            {
                let before = crate::process::process_birth_identity(record.pid)
                    .ok()
                    .flatten();
                let peer = crate::process::worker::peer_pid_from_socket(
                    &crate::process::worker::control_socket_sibling(&record.socket_path),
                );
                let after = crate::process::process_birth_identity(record.pid)
                    .ok()
                    .flatten();
                if peer == Some(record.pid) && before == after {
                    after
                } else {
                    None
                }
            } else {
                None
            };
            object.insert("runner_identity".into(), serde_json::to_value(identity)?);
            true
        } else if is_proof {
            let mut changed = false;
            for field in ["epoch", "launch_epoch"] {
                if !object.contains_key(field) {
                    object.insert(field.into(), Value::Null);
                    changed = true;
                }
            }
            changed
        } else {
            false
        };
        if changed {
            crate::session::backup_before_migration(&path)?;
            crate::session::atomic_write(&path, &serde_json::to_vec_pretty(&value)?)?;
            tracing::info!(path = %path.display(), "v040: migrated worker ownership evidence");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn migration_attests_only_live_control_owners_and_preserves_unknown_evidence() {
        let root = tempfile::TempDir::with_prefix_in("aoe-migrate-", "/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        let app = crate::session::get_app_dir().unwrap();
        let workers = crate::process::worker_registry::workers_dir().unwrap();
        let socket = workers.join("migration.sock");
        let control = crate::process::worker::control_socket_sibling(&socket);
        let listener = std::os::unix::net::UnixListener::bind(&control).unwrap();
        let record = crate::process::worker_registry::WorkerRecord::new(
            "migration".into(),
            std::process::id(),
            socket,
            "agent".into(),
            "agent".into(),
            "/tmp".into(),
            None,
            Vec::new(),
            Vec::new(),
            None,
            None,
        );
        let mut old = serde_json::to_value(record).unwrap();
        old.as_object_mut().unwrap().remove("runner_identity");
        old["runner_version"] = serde_json::json!(6);
        old["extension"] = serde_json::json!({"keep":"opaque"});
        let bytes = serde_json::to_vec(&old).unwrap();
        let path = workers.join("migration.json");
        fs::write(&path, &bytes).unwrap();
        let corrupt = workers.join("corrupt.json");
        fs::write(&corrupt, b"{").unwrap();
        let proof = workers.join(".owned.prime-proof");
        fs::write(&proof, serde_json::to_vec(&serde_json::json!({"route":"owned", "processes":[], "settled":true, "extension":42})).unwrap()).unwrap();
        let outside = root.path().join("outside.json");
        fs::write(&outside, b"unidentified ownership").unwrap();
        std::os::unix::fs::symlink(&outside, workers.join("link.json")).unwrap();
        run_in(&app).unwrap();
        let migrated_bytes = fs::read(&path).unwrap();
        let migrated: Value = serde_json::from_slice(&migrated_bytes).unwrap();
        assert_eq!(
            migrated["runner_identity"],
            serde_json::to_value(
                crate::process::process_birth_identity(std::process::id()).unwrap()
            )
            .unwrap()
        );
        assert_eq!(migrated["extension"], old["extension"]);
        let receipt: Value = serde_json::from_slice(&fs::read(&proof).unwrap()).unwrap();
        assert!(receipt["epoch"].is_null() && receipt["launch_epoch"].is_null());
        assert_eq!(receipt["extension"], 42);
        assert_eq!(fs::read(&corrupt).unwrap(), b"{");
        assert_eq!(fs::read(&outside).unwrap(), b"unidentified ownership");
        assert!(fs::symlink_metadata(workers.join("link.json"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(fs::read_dir(&workers)
            .unwrap()
            .flatten()
            .any(|entry| entry.path() != path
                && fs::read(entry.path()).is_ok_and(|saved| saved == bytes)));
        drop(listener);
        run_in(&app).unwrap();
        assert_eq!(fs::read(&path).unwrap(), migrated_bytes);
    }
}
