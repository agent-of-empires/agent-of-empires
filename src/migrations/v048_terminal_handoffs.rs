use anyhow::{Context, Result};
use serde_json::Value;
use std::path::Path;

pub fn run() -> Result<()> {
    for path in super::sessions_file::session_files(&crate::session::get_app_dir()?)? {
        migrate_file(&path)?;
    }
    tracing::info!(target: "migrations.v048", "Initialized explicit terminal handoff state");
    Ok(())
}

fn migrate_file(path: &Path) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "terminal handoff migration requires a regular sessions file: {}",
        path.display()
    );
    let parent = path.parent().context("sessions file has no parent")?;
    let _lock =
        crate::session::acquire_storage_flock(parent, crate::session::STORAGE_LOCK_FILENAME)?;
    let bytes = std::fs::read(path)?;
    let mut value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(target: "migrations.v048", path = %path.display(), %error, "Preserved unreadable session component; repair it and run aoe migrate");
            return Ok(());
        }
    };
    let Some(rows) = value.as_array_mut() else {
        tracing::warn!(target: "migrations.v048", path = %path.display(), "Preserved invalid session component; repair it and run aoe migrate");
        return Ok(());
    };
    let mut changed = false;
    let mut invalid_rows = false;
    for row in rows {
        let Some(object) = row.as_object_mut() else {
            invalid_rows = true;
            continue;
        };
        if let Some(intent) = object
            .get("terminal_handoff")
            .filter(|value| !value.is_null())
        {
            let intent: crate::session::TerminalHandoffIntent =
                serde_json::from_value(intent.clone())
                    .context("existing terminal handoff authority is malformed")?;
            anyhow::ensure!(
                intent.target_generation <= intent.generation,
                "terminal handoff target generation exceeds ownership generation"
            );
            anyhow::ensure!(
                intent
                    .launch
                    .as_ref()
                    .is_none_or(|launch| launch.generation <= intent.generation),
                "terminal handoff attempt generation exceeds ownership generation"
            );
            if intent.unborn_fence.is_some() {
                intent.validate_unborn_fence()?;
            }
        } else if !object.contains_key("terminal_handoff") {
            object.insert("terminal_handoff".into(), Value::Null);
            changed = true;
        }
    }
    if invalid_rows {
        tracing::warn!(target: "migrations.v048", path = %path.display(), "Preserved invalid session rows; repair them and run aoe migrate");
        return Ok(());
    }
    if changed {
        crate::session::backup_before_migration(path)?;
        crate::session::atomic_write(path, &serde_json::to_vec_pretty(&value)?)?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    #[serial_test::serial]
    fn unreadable_components_keep_bytes_while_healthy_profiles_upgrade() -> Result<()> {
        for invalid in [
            b"{".as_slice(),
            b"{}",
            b"[null]",
            br#"[{"id":"old"},null]"#,
            &[0xff],
        ] {
            let home = tempfile::tempdir()?;
            let _home = crate::session::test_support::isolate_app_dir_at(home.path());
            let app = crate::session::get_app_dir()?;
            let healthy = crate::session::Storage::new_unwatched("healthy")?;
            let damaged = crate::session::Storage::new_unwatched("damaged")?;
            let mut row = serde_json::to_value(crate::session::Instance::new("old", "/tmp/repo"))?;
            row["terminal_scopes"] = Value::Null;
            row["opaque"] = json!({"keep":17});
            let original = serde_json::to_vec(&json!([row]))?;
            std::fs::write(healthy.sessions_path(), &original)?;
            std::fs::write(damaged.sessions_path(), invalid)?;
            std::fs::write(app.join(".schema_version"), "47")?;

            crate::migrations::run_migrations()?;

            assert_eq!(
                crate::migrations::get_current_version(),
                crate::migrations::CURRENT_VERSION
            );
            assert_eq!(std::fs::read(damaged.sessions_path())?, invalid);
            assert!(crate::session::migration_backups(damaged.sessions_path())?.is_empty());
            let migrated: Value = serde_json::from_slice(&std::fs::read(healthy.sessions_path())?)?;
            assert_eq!(migrated[0]["terminal_scopes"], Value::Null);
            assert_eq!(migrated[0]["terminal_handoff"], Value::Null);
            assert!(migrated[0]
                .as_object()
                .unwrap()
                .contains_key("terminal_handoff"));
            assert_eq!(migrated[0]["opaque"], json!({"keep":17}));
            assert!(crate::session::migration_backups(healthy.sessions_path())?
                .iter()
                .any(|(_, backup)| std::fs::read(backup).is_ok_and(|bytes| bytes == original)));
            let committed = std::fs::read(healthy.sessions_path())?;
            run()?;
            assert_eq!(std::fs::read(healthy.sessions_path())?, committed);
            assert_eq!(std::fs::read(damaged.sessions_path())?, invalid);
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn handoff_authority_survives_and_invalid_proof_blocks_component_rewrites() -> Result<()> {
        let home = tempfile::tempdir()?;
        let _home = crate::session::test_support::isolate_app_dir_at(home.path());
        let app = crate::session::get_app_dir()?;
        let path = app.join("sessions.json");
        let mut row = crate::session::Instance::new("pending", "/tmp/repo");
        row.lifecycle_generation = 7;
        let intent = crate::session::TerminalHandoffIntent {
            id: uuid::Uuid::new_v4(),
            profile: "default".into(),
            profile_identity: (0, 0),
            generation: 7,
            target_generation: 7,
            stage: crate::session::TerminalHandoffStage::RetirementPending,
            context: crate::session::TerminalHandoffContext::Discard,
            source_acp_session_id: Some("original-acp".into()),
            source_event_seq: 17,
            final_event_seq: None,
            source_conversation: row.conversation_state(),
            runner: crate::session::purge_owners::RunnerCapture::Unresolved {
                error: "retained original authority".into(),
            },
            unborn_fence: None,
            terminal: None,
            launch_required: false,
            launch_started: false,
            launch: None,
        };
        let mut pending = serde_json::to_value(&row)?;
        pending["terminal_handoff"] = serde_json::to_value(&intent)?;
        let earlier = serde_json::to_value(crate::session::Instance::new("earlier", "/tmp/repo"))?;
        let original = serde_json::to_vec(&json!([earlier, pending]))?;
        std::fs::write(&path, &original)?;
        migrate_file(&path)?;
        let committed = std::fs::read(&path)?;
        let migrated: Value = serde_json::from_slice(&committed)?;
        assert_eq!(migrated[1], pending);
        assert!(crate::session::migration_backups(&path)?
            .iter()
            .any(|(_, backup)| std::fs::read(backup).is_ok_and(|bytes| bytes == original)));
        migrate_file(&path)?;
        assert_eq!(std::fs::read(&path)?, committed);

        for corruption in 0..4 {
            let mut invalid = pending.clone();
            match corruption {
                0 => invalid["terminal_handoff"] = json!({"id":"not-a-uuid"}),
                1 => invalid["terminal_handoff"]["target_generation"] = json!(8),
                2 => {
                    invalid["terminal_handoff"]["launch"] = json!({
                        "generation":8, "prior":row.conversation_state(), "launched":row.conversation_state(),
                        "capture_started_at":null, "omp_capture_metadata":null, "confirm_desired_conversation":false,
                    })
                }
                3 => {
                    invalid["terminal_handoff"]["unborn_fence"] = json!({
                        "producer_epoch":3, "producer_id":uuid::Uuid::new_v4(),
                        "source_generation":5, "fence_generation":7,
                    })
                }
                _ => unreachable!(),
            }
            let bytes = serde_json::to_vec(&json!([earlier, null, invalid]))?;
            std::fs::write(&path, &bytes)?;
            std::fs::write(app.join(".schema_version"), "47")?;
            let backups = crate::session::migration_backups(&path)?.len();
            assert!(crate::migrations::run_migrations().is_err());
            assert_eq!(crate::migrations::get_current_version(), 47);
            assert_eq!(std::fs::read(&path)?, bytes);
            assert_eq!(crate::session::migration_backups(&path)?.len(), backups);
        }
        Ok(())
    }
}
