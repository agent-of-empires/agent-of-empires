use anyhow::{Context, Result};
use serde_json::Value;
use std::path::Path;

pub fn run() -> Result<()> {
    for path in super::sessions_file::session_files(&crate::session::get_app_dir()?)? {
        validate_file(&path)?;
    }
    tracing::info!(target: "migrations.v054", "Validated retained ACP establishment authority");
    Ok(())
}

fn validate_file(path: &Path) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "ACP establishment migration requires a regular sessions file: {}",
        path.display()
    );
    let parent = path.parent().context("sessions file has no parent")?;
    let _lock =
        crate::session::acquire_storage_flock(parent, crate::session::STORAGE_LOCK_FILENAME)?;
    let bytes = std::fs::read(path)?;
    // Keep unreadable legacy components quarantined, but validate every authority
    // that can be inspected before allowing the upgrade chain to proceed.
    let document: Value = match serde_json::from_slice(&bytes) {
        Ok(document) => document,
        Err(error) => {
            tracing::warn!(target: "migrations.v054", path = %path.display(), %error, "Preserved unreadable session component; repair it and run aoe migrate");
            return Ok(());
        }
    };
    let Some(rows) = document.as_array() else {
        tracing::warn!(target: "migrations.v054", path = %path.display(), "Preserved invalid session component; repair it and run aoe migrate");
        return Ok(());
    };
    for (index, row) in rows.iter().enumerate() {
        let Some(authority) = row
            .get("acp_establishment")
            .filter(|value| !value.is_null())
        else {
            continue;
        };
        (|| -> Result<()> {
            let _: crate::acp::establishment::AcpEstablishment =
                serde_json::from_value(authority.clone())
                    .context("existing ACP establishment authority is malformed")?;
            let instance: crate::session::Instance = serde_json::from_value(row.clone())
                .context("row retaining ACP establishment authority cannot be decoded")?;
            anyhow::ensure!(
                parent.parent().and_then(Path::file_name) == Some(std::ffi::OsStr::new("profiles")),
                "ACP establishment authority is outside its physical profile"
            );
            let profile = parent
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .context("profile is not UTF-8")?;
            let storage = crate::session::Storage::open_inventory_reference(
                profile,
                crate::file_watch::FileWatchService::noop(),
            )?;
            anyhow::ensure!(
                storage.sessions_path() == path,
                "ACP establishment authority changed physical profile"
            );
            instance.validate_acp_establishment(&storage)
        })()
        .with_context(|| {
            format!(
                "invalid ACP establishment at row {index} in {}",
                path.display()
            )
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_and_null_authority_preserve_legacy_bytes() -> Result<()> {
        for null in [false, true] {
            let root = tempfile::tempdir()?;
            let path = root.path().join("sessions.json");
            let mut row = serde_json::to_value(crate::session::Instance::new("legacy", "/tmp"))?;
            row.as_object_mut().unwrap().remove("acp_establishment");
            if null {
                row["acp_establishment"] = Value::Null;
            }
            row["opaque"] = serde_json::json!({"keep":17});
            let bytes = serde_json::to_vec_pretty(&vec![row])?;
            std::fs::write(&path, &bytes)?;
            for _ in 0..2 {
                validate_file(&path)?;
                assert_eq!(std::fs::read(&path)?, bytes);
                assert!(crate::session::migration_backups(&path)?.is_empty());
            }
        }
        Ok(())
    }

    #[test]
    fn quarantined_components_preserve_bytes_without_blocking_readable_files() -> Result<()> {
        let root = tempfile::tempdir()?;
        let damaged = root.path().join("damaged.json");
        let healthy = root.path().join("healthy.json");
        let original = serde_json::to_vec(&vec![crate::session::Instance::new("healthy", "/tmp")])?;
        std::fs::write(&healthy, &original)?;
        for invalid in [
            b"{".as_slice(),
            b"{}",
            b"[null]",
            br#"[{"id":"old"},null]"#,
            &[0xff],
        ] {
            std::fs::write(&damaged, invalid)?;
            for _ in 0..2 {
                validate_file(&damaged)?;
                validate_file(&healthy)?;
                assert_eq!(std::fs::read(&damaged)?, invalid);
                assert_eq!(std::fs::read(&healthy)?, original);
                assert!(crate::session::migration_backups(&damaged)?.is_empty());
                assert!(crate::session::migration_backups(&healthy)?.is_empty());
            }
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn present_authority_is_validated_even_in_otherwise_undecodable_rows() -> Result<()> {
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("default")?;
        let mut row = crate::session::Instance::new("pending", "/tmp");
        row.view = crate::session::View::Structured;
        row.acp_session_id = Some("migration-native".into());
        row.arm_acp_establishment(&storage, None)?;
        let valid = serde_json::to_value(&row)?;
        let original = serde_json::to_vec(&vec![valid.clone()])?;
        std::fs::write(storage.sessions_path(), &original)?;
        validate_file(storage.sessions_path())?;
        assert_eq!(std::fs::read(storage.sessions_path())?, original);
        for corruption in 0..5 {
            let mut invalid = valid.clone();
            match corruption {
                0 => invalid["acp_establishment"]["attempt"]["id"] = serde_json::json!("bad"),
                1 => invalid["acp_establishment"]["attempt"]["generation"] = serde_json::json!(91),
                2 => {
                    invalid["acp_establishment"]["attempt"]["target"] = serde_json::json!({"operation":"load","session_id":"different","import":false})
                }
                3 => invalid["acp_establishment"]["state"] = serde_json::json!("future"),
                4 => invalid["title"] = serde_json::json!([]),
                _ => unreachable!(),
            }
            let bytes = serde_json::to_vec(&serde_json::json!([null, invalid]))?;
            std::fs::write(storage.sessions_path(), &bytes)?;
            assert!(
                validate_file(storage.sessions_path()).is_err(),
                "corruption {corruption}"
            );
            assert!(storage.load().is_err(), "corruption {corruption}");
            assert_eq!(std::fs::read(storage.sessions_path())?, bytes);
            assert!(crate::session::migration_backups(storage.sessions_path())?.is_empty());
        }
        Ok(())
    }
}
