//! Preserve old cleanup uncertainty rather than invent exact scope authority.

use std::path::Path;

use anyhow::{Context, Result};
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
        let mut rows: Value = serde_json::from_str(&raw)?;
        let rows = rows.as_array_mut().context("invalid sessions document")?;
        let mut changed = false;
        for row in rows.iter_mut() {
            let row = row.as_object_mut().context("invalid session row")?;
            if let Some(scopes) = row.get("terminal_scopes") {
                serde_json::from_value::<
                    Option<Vec<crate::process::worker_registry::ScopeOwnership>>,
                >(scopes.clone())?;
            } else {
                row.insert("terminal_scopes".into(), Value::Null);
                changed = true;
            }
        }
        if changed {
            crate::session::backup_before_migration(&path)?;
            file.replace(&serde_json::to_vec(rows)?)?;
            file.sync_parent()?;
            tracing::info!(path = %path.display(), "Preserved unresolved legacy terminal scope ownership");
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
                let mut record: Value = serde_json::from_str(&raw)?;
                let object = record.as_object_mut().context("invalid worker record")?;
                if let Some(scope) = object.get("scope") {
                    serde_json::from_value::<crate::process::worker_registry::ScopeOwnership>(
                        scope.clone(),
                    )?;
                    serde_json::from_value::<crate::process::worker_registry::WorkerRecord>(
                        record.clone(),
                    )?;
                    return Ok(());
                }
                object.insert("scope".into(), json!({"state": "unresolved"}));
                serde_json::from_value::<crate::process::worker_registry::WorkerRecord>(
                    record.clone(),
                )?;
                crate::session::backup_before_migration(&app.join("acp-workers").join(&name))?;
                file.replace(&serde_json::to_vec(&record)?)?;
                file.sync_parent()?;
                tracing::info!(
                    session = id,
                    "Preserved unresolved legacy runner scope ownership"
                );
                Ok(())
            })?;
        }
    }
    let file = root.bind_file(crate::session::purge_owners::FILE_NAME.as_ref())?;
    let (lock, path) = file.open_sidecar()?;
    let _lock = crate::session::acquire_open_storage_flock(lock, &path)?;
    let Some(raw) = file.read()? else {
        return Ok(());
    };
    let mut journal: Value = serde_json::from_str(&raw)?;
    anyhow::ensure!(
        journal["version"] == 2,
        "Unsupported pending purge ownership version"
    );
    let owners = journal["owners"]
        .as_array_mut()
        .context("invalid pending purge ownership")?;
    let mut changed = false;
    for owner in owners {
        let runner = owner["runner"]
            .as_object_mut()
            .context("invalid captured runner")?;
        match runner.get("state").and_then(Value::as_str) {
            Some("captured" | "uncaptured") => {
                if let Some(scopes) = runner.get("scopes") {
                    serde_json::from_value::<Vec<crate::process::worker_registry::ScopeOwnership>>(
                        scopes.clone(),
                    )?;
                } else {
                    runner.insert("scopes".into(), json!([{"state": "unresolved"}]));
                    changed = true;
                }
            }
            Some("unresolved") => {}
            _ => anyhow::bail!("invalid captured runner state"),
        }
    }
    let encoded = serde_json::to_vec(&journal)?;
    crate::session::purge_owners::validate_journal_for_migration(std::str::from_utf8(&encoded)?)?;
    if changed {
        crate::session::backup_before_migration(
            &app.join(crate::session::purge_owners::FILE_NAME),
        )?;
        file.replace(&encoded)?;
        file.sync_parent()?;
        tracing::info!("Preserved unresolved legacy purge scope ownership");
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
        assert_eq!(std::fs::read(app.join(".schema_version"))?, b"45");
        assert_eq!(std::fs::read(&record_path)?, record_bytes);
        assert_eq!(std::fs::read(&journal_path)?, repaired);
        crate::migrations::run_migrations()?;
        assert_eq!(std::fs::read(&record_path)?, record_bytes);
        assert_eq!(std::fs::read(journal_path)?, repaired);
        Ok(())
    }
}
