use anyhow::{Context, Result};
use serde_json::Value;
use std::path::Path;

pub fn run() -> Result<()> {
    for path in super::sessions_file::session_files(&crate::session::get_app_dir()?)? {
        migrate_file(&path)?;
    }
    tracing::info!(target: "migrations.v048", "Validated retained terminal handoff authority");
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
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(target: "migrations.v048", path = %path.display(), %error, "Preserved unreadable session component; repair it and run aoe migrate");
            return Ok(());
        }
    };
    let Some(rows) = value.as_array() else {
        tracing::warn!(target: "migrations.v048", path = %path.display(), "Preserved invalid session component; repair it and run aoe migrate");
        return Ok(());
    };
    let mut invalid_rows = false;
    for row in rows {
        let Some(object) = row.as_object() else {
            invalid_rows = true;
            continue;
        };
        if let Some(intent) = object
            .get("terminal_handoff")
            .filter(|value| !value.is_null())
        {
            let intent: crate::session::TerminalHandoffIntent =
                serde::Deserialize::deserialize(intent)
                    .context("existing terminal handoff authority is malformed")?;
            intent.validate_relationships()?;
        }
    }
    if invalid_rows {
        tracing::warn!(target: "migrations.v048", path = %path.display(), "Preserved invalid session rows; repair them and run aoe migrate");
        return Ok(());
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
            if invalid == b"{}" {
                row["terminal_handoff"] = Value::Null;
            }
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
            assert_eq!(std::fs::read(healthy.sessions_path())?, original);
            assert!(crate::session::migration_backups(healthy.sessions_path())?.is_empty());
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
        assert_eq!(committed, original);
        assert!(crate::session::migration_backups(&path)?.is_empty());
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

            std::fs::write(app.join(".schema_version"), "48")?;
            let bytes = serde_json::to_vec(&json!([earlier, invalid]))?;
            std::fs::write(&path, &bytes)?;
            let storage = crate::session::Storage::new_for_test_path("default", path.clone());
            assert!(storage.load().is_err(), "corruption {corruption}");
            assert!(
                storage.load_instances_readonly().is_err(),
                "corruption {corruption}"
            );
            assert!(
                storage.load_complete_with_groups().is_err(),
                "corruption {corruption}"
            );
            let entered = std::cell::Cell::new(false);
            assert!(storage
                .update(|_, _| {
                    entered.set(true);
                    Ok(())
                })
                .is_err());
            assert!(!entered.get());
            assert_eq!(std::fs::read(&path)?, bytes);
            assert!(!app.join("sessions.corrupt.jsonl").exists());
        }
        Ok(())
    }
}
