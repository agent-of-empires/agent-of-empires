//! Preserve old cleanup uncertainty rather than invent exact scope authority.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

pub fn run() -> Result<()> {
    let app = crate::session::get_app_dir()?;
    let root = crate::session::AnchoredDir::open(&app)?;
    // Session files retain exact terminal authority even when their pane is gone.
    let _identity = crate::session::acquire_session_identity_lock()?;
    for path in super::sessions_file::session_files(&app)? {
        if !path.try_exists()? {
            continue;
        }
        let _storage = crate::session::acquire_storage_flock(
            path.parent().context("session file has no parent")?,
            crate::session::STORAGE_LOCK_FILENAME,
        )?;
        let file = crate::session::ResolvedDataFile::open(&path)?;
        let Some(raw) = file.read()? else {
            continue;
        };
        let mut document: Value = match serde_json::from_str(&raw) {
            Ok(document) => document,
            Err(error) => {
                tracing::warn!(target: "migrations.v045", path = %path.display(), %error, "Quarantined unreadable session component; repair it and run aoe migrate");
                continue;
            }
        };
        let Some(rows) = document.as_array_mut() else {
            tracing::warn!(target: "migrations.v045", path = %path.display(), "Quarantined invalid session component; repair it and run aoe migrate");
            continue;
        };
        for row in rows.iter().filter_map(Value::as_object) {
            if let Some(scopes) = row.get("terminal_scopes") {
                Option::<Vec<crate::process::worker_registry::ScopeOwnership>>::deserialize(
                    scopes,
                )?;
            }
        }
        if rows
            .iter()
            .any(|row| crate::session::Instance::deserialize(row).is_err())
        {
            tracing::warn!(target: "migrations.v045", path = %path.display(), "Quarantined invalid session rows; repair them and run aoe migrate");
            continue;
        }
        let mut changed = false;
        for row in rows.iter_mut().filter_map(Value::as_object_mut) {
            if !row.contains_key("terminal_scopes") {
                row.insert("terminal_scopes".into(), Value::Null);
                changed = true;
            }
        }
        if changed {
            crate::session::backup_before_migration(&path)?;
            file.replace(&serde_json::to_vec(rows)?)?;
            file.sync_parent()?;
            tracing::info!(target: "migrations.v045", path = %path.display(), "Preserved unresolved legacy terminal scope ownership");
        }
    }

    if app.join("acp-workers").try_exists()? {
        let workers = root.child(Path::new("acp-workers"))?;
        for name in workers.read_dir(Path::new(""), usize::MAX)? {
            let path = Path::new(&name);
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .context("invalid worker record name")?;
            crate::process::worker_registry::with_registry_lock(id, || {
                let file = workers.bind_file(&name)?;
                let Some(raw) = file.read()? else {
                    return Ok(());
                };
                let mut record: Value = match serde_json::from_str(&raw) {
                    Ok(record) => record,
                    Err(error) => {
                        tracing::warn!(target: "migrations.v045", session = id, %error, "Quarantined unreadable worker component; repair it and run aoe migrate");
                        return Ok(());
                    }
                };
                let Some(object) = record.as_object_mut() else {
                    tracing::warn!(
                        target: "migrations.v045",
                        session = id,
                        "Quarantined invalid worker component; repair it and run aoe migrate"
                    );
                    return Ok(());
                };
                if let Some(identity) = object.get("runner_identity") {
                    Option::<crate::process::ProcessBirthIdentity>::deserialize(identity)?;
                }
                if let Some(runtime) = object.get("prime_runtime") {
                    Option::<crate::process::prime::PrimeRuntime>::deserialize(runtime)?;
                }
                if let Some(pid) = object.get("pid") {
                    u32::deserialize(pid)?;
                }
                if let Some(generation) = object.get("generation") {
                    u64::deserialize(generation)?;
                }
                if let Some(scope) = object.get("scope") {
                    serde_json::from_value::<crate::process::worker_registry::ScopeOwnership>(
                        scope.clone(),
                    )?;
                    if crate::process::worker_registry::WorkerRecord::deserialize(&record).is_err()
                    {
                        tracing::warn!(
                            target: "migrations.v045",
                            session = id,
                            "Quarantined invalid worker fields; repair them and run aoe migrate"
                        );
                    }
                    return Ok(());
                }
                object.insert("scope".into(), json!({"state": "unresolved"}));
                if crate::process::worker_registry::WorkerRecord::deserialize(&record).is_err() {
                    tracing::warn!(target: "migrations.v045", session = id, "Deferred unresolved worker scope; repair the component and run aoe migrate");
                    return Ok(());
                }
                crate::session::backup_before_migration(&app.join("acp-workers").join(&name))?;
                file.replace(&serde_json::to_vec(&record)?)?;
                file.sync_parent()?;
                tracing::info!(
                    target: "migrations.v045",
                    session = id,
                    "Preserved unresolved legacy runner scope ownership"
                );
                Ok(())
            })?;
        }
    }
    migrate_journal(&root, &app)
}

pub(super) fn migrate_journal(root: &crate::session::AnchoredDir, app: &Path) -> Result<()> {
    let file = root.bind_file(crate::session::purge_owners::FILE_NAME.as_ref())?;
    let (lock, path) = file.open_sidecar()?;
    let _lock = crate::session::acquire_open_storage_flock(lock, &path)?;
    let Some(raw) = file.read()? else {
        return Ok(());
    };
    let encoded = crate::session::purge_owners::upcast_journal(
        &raw,
        crate::session::purge_owners::JournalInput::Migration,
    )?;
    if encoded.as_ref() != raw.as_bytes() {
        crate::session::backup_before_migration(
            &app.join(crate::session::purge_owners::FILE_NAME),
        )?;
        file.replace(&encoded)?;
        file.sync_parent()?;
        tracing::info!(target: "migrations.v045", "Preserved unresolved legacy purge scope ownership");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn old_owners_remain_unresolved_and_repeated_runs_preserve_bytes() -> Result<()> {
        let home = tempfile::tempdir()?;
        let _home = crate::session::test_support::isolate_app_dir_at(home.path());
        let app = crate::session::get_app_dir()?;
        use crate::process::worker_registry::{self, WorkerRecord};
        use crate::session::purge_owners::{PurgeCapture, PurgeOwner};
        let record = WorkerRecord::new(
            "old".into(),
            42,
            home.path().join("old.sock"),
            "agent".into(),
            "agent".into(),
            home.path().to_owned(),
            None,
            vec![],
            vec![],
            None,
            Some("default".into()),
        )
        .with_generation(17);
        worker_registry::save(&record)?;
        crate::session::purge_owners::initialize(&app)?;
        let mut row = crate::session::Instance::new("old", home.path().to_str().unwrap());
        row.id = "old".into();
        row.source_profile = "default".into();
        let store =
            crate::session::Storage::new_for_test_path("default", app.join("sessions.json"));
        let sessions_path = app.join("sessions.json");
        let mut legacy_row = serde_json::to_value(&row)?;
        legacy_row
            .as_object_mut()
            .unwrap()
            .remove("terminal_scopes");
        std::fs::write(&sessions_path, serde_json::to_vec(&json!([legacy_row]))?)?;
        drop(PurgeOwner::record(&store, &row, PurgeCapture::new(&row)?)?);
        let record_path = app.join("acp-workers/old.json");
        let mut record: Value = serde_json::from_slice(&std::fs::read(&record_path)?)?;
        record.as_object_mut().unwrap().remove("scope");
        std::fs::write(&record_path, serde_json::to_vec(&record)?)?;
        let journal_path = app.join(crate::session::purge_owners::FILE_NAME);
        let mut journal: Value = serde_json::from_slice(&std::fs::read(&journal_path)?)?;
        let token = journal["owners"][0]["token"].clone();
        journal["version"] = json!(2);
        journal["owners"][0]["runner"]
            .as_object_mut()
            .unwrap()
            .remove("prime");
        journal["owners"][0]["runner"]
            .as_object_mut()
            .unwrap()
            .remove("birth_fingerprint");
        journal["owners"][0]["runner"]
            .as_object_mut()
            .unwrap()
            .remove("scopes");
        std::fs::write(&journal_path, serde_json::to_vec(&journal)?)?;
        run()?;
        let record: Value = serde_json::from_slice(&std::fs::read(&record_path)?)?;
        let journal: Value = serde_json::from_slice(&std::fs::read(&journal_path)?)?;
        let rows: Value = serde_json::from_slice(&std::fs::read(&sessions_path)?)?;
        assert_eq!(rows[0]["id"], "old");
        assert!(rows[0]["terminal_scopes"].is_null());
        assert_eq!(record["pid"], 42);
        assert_eq!(record["generation"], 17);
        assert_eq!(record["scope"]["state"], "unresolved");
        assert_eq!(journal["owners"][0]["token"], token);
        assert_eq!(
            journal["owners"][0]["runner"]["scopes"],
            json!([{"state":"unresolved"}])
        );
        assert_eq!(journal["version"], 3);
        assert_eq!(
            journal["owners"][0]["runner"]["prime"],
            json!({"state":"unresolved"})
        );
        assert!(journal["owners"][0]["runner"]["birth_fingerprint"].is_null());
        let record_bytes = std::fs::read(&record_path)?;
        let journal_bytes = std::fs::read(&journal_path)?;
        let sessions_bytes = std::fs::read(&sessions_path)?;
        run()?;
        assert_eq!(std::fs::read(record_path)?, record_bytes);
        assert_eq!(std::fs::read(journal_path)?, journal_bytes);
        assert_eq!(std::fs::read(sessions_path)?, sessions_bytes);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn malformed_journal_does_not_advance_the_marker_or_clobber_committed_scope_evidence(
    ) -> Result<()> {
        let home = tempfile::tempdir()?;
        let _home = crate::session::test_support::isolate_app_dir_at(home.path());
        let app = crate::session::get_app_dir()?;
        use crate::process::worker_registry::{self, ScopeOwnership, WorkerRecord};
        let mut record = WorkerRecord::new(
            "committed".into(),
            42,
            home.path().join("committed.sock"),
            "agent".into(),
            "agent".into(),
            home.path().to_owned(),
            None,
            vec![],
            vec![],
            None,
            Some("default".into()),
        )
        .with_generation(17);
        record.scope = ScopeOwnership::Owned {
            unit: "aoe-session-committed-known.scope".into(),
        };
        worker_registry::save(&record)?;
        let record_path = app.join("acp-workers/committed.json");
        let record_bytes = std::fs::read(&record_path)?;
        let journal_path = app.join(crate::session::purge_owners::FILE_NAME);
        let malformed = br#"{"version":2,"owners":[{"token":"not-a-complete-owner"}]}"#;
        std::fs::write(&journal_path, malformed)?;
        std::fs::write(app.join(".schema_version"), b"44")?;
        assert!(crate::migrations::run_migrations().is_err());
        assert_eq!(std::fs::read(app.join(".schema_version"))?, b"44");
        assert_eq!(std::fs::read(&record_path)?, record_bytes);
        assert_eq!(std::fs::read(&journal_path)?, malformed);
        let repaired = br#"{"version":2,"owners":[]}"#;
        std::fs::write(&journal_path, repaired)?;
        crate::migrations::run_migrations()?;
        assert_eq!(
            std::fs::read_to_string(app.join(".schema_version"))?,
            crate::migrations::current_schema_version().to_string()
        );
        assert_eq!(std::fs::read(&record_path)?, record_bytes);
        let migrated = std::fs::read(&journal_path)?;
        let value: Value = serde_json::from_slice(&migrated)?;
        assert_eq!(value, json!({"version":3,"owners":[]}));
        assert!(crate::session::migration_backups(&journal_path)?
            .iter()
            .any(|(_, path)| std::fs::read(path).is_ok_and(|bytes| bytes == repaired)));
        crate::migrations::run_migrations()?;
        assert_eq!(std::fs::read(&record_path)?, record_bytes);
        assert_eq!(std::fs::read(journal_path)?, migrated);
        Ok(())
    }
}
