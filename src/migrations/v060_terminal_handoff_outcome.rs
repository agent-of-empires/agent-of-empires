//! Record each pending terminal handoff's outcome, which readers used to infer from its target.

use anyhow::{Context, Result};
use serde_json::Value;
use std::path::Path;

pub fn run() -> Result<()> {
    run_in(&crate::session::get_app_dir()?)
}

fn run_in(app_dir: &Path) -> Result<()> {
    for path in super::sessions_file::session_files(app_dir)? {
        migrate_file(&path)?;
    }
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
    let directory = path.parent().context("sessions file has no parent")?;
    let _lock =
        crate::session::acquire_storage_flock(directory, crate::session::STORAGE_LOCK_FILENAME)?;
    let bytes = std::fs::read(path)?;
    let mut rows: Value = match serde_json::from_slice(&bytes) {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(target: "migrations.v060", path = %path.display(), %error, "Preserved unreadable session component; repair it and run aoe migrate");
            return Ok(());
        }
    };
    let Some(rows) = rows.as_array_mut() else {
        tracing::warn!(target: "migrations.v060", path = %path.display(), "Preserved invalid session component; repair it and run aoe migrate");
        return Ok(());
    };
    let mut recorded = 0;
    for (index, row) in rows.iter_mut().enumerate() {
        let Some(intent) = row
            .get_mut("terminal_handoff")
            .filter(|value| !value.is_null())
        else {
            continue;
        };
        let fields = intent.as_object_mut().with_context(|| {
            format!(
                "invalid terminal handoff at session row {index} in {}",
                path.display()
            )
        })?;
        let recorded_already = fields.get("outcome").cloned();
        let outcome = (|| -> Result<Value> {
            let outcome = match recorded_already.clone() {
                Some(outcome) => outcome,
                None => serde_json::to_value(outcome_of(fields)?)?,
            };
            // Validate a copy of every handoff, as v048 does, so a malformed one is refused before
            // any other row is rewritten.
            let mut copy = fields.clone();
            copy.insert("outcome".into(), outcome.clone());
            let intent: crate::session::TerminalHandoffIntent =
                serde_json::from_value(Value::Object(copy))
                    .context("existing terminal handoff authority is malformed")?;
            intent.validate_relationships()?;
            Ok(outcome)
        })()
        .with_context(|| {
            format!(
                "invalid terminal handoff at session row {index} in {}",
                path.display()
            )
        })?;
        if recorded_already.is_none() {
            fields.insert("outcome".into(), outcome);
            recorded += 1;
        }
    }
    if recorded > 0 {
        crate::session::backup_before_migration(path)?;
        crate::session::atomic_write(path, serde_json::to_vec_pretty(&rows)?.as_slice())?;
        tracing::info!(target: "migrations.v060", path = %path.display(), recorded, "Recorded terminal handoff outcomes");
    }
    Ok(())
}

/// The outcome readers inferred before it was recorded.
fn outcome_of(
    fields: &serde_json::Map<String, Value>,
) -> Result<crate::session::TerminalHandoffOutcome> {
    let context: crate::session::TerminalHandoffContext =
        serde_json::from_value(fields.get("context").cloned().unwrap_or(Value::Null))
            .context("terminal handoff context is malformed")?;
    let terminal: Option<crate::session::ConversationState> =
        serde_json::from_value(fields.get("terminal").cloned().unwrap_or(Value::Null))
            .context("terminal handoff target is malformed")?;
    Ok(crate::session::TerminalHandoffOutcome::of(
        &context,
        terminal.as_ref(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{
        Instance, ResumeIntent, TerminalHandoffContext, TerminalHandoffIntent,
        TerminalHandoffOutcome,
    };
    use serde_json::json;

    fn pending(row: &Instance, context: TerminalHandoffContext) -> TerminalHandoffIntent {
        let mut intent = crate::session::test_support::launch_owed_handoff(row);
        intent.stage = crate::session::TerminalHandoffStage::RetirementPending;
        intent.outcome = TerminalHandoffOutcome::of(&context, None);
        intent.context = context;
        intent
    }

    /// A row as a build before v060 wrote it: its handoff has no `outcome`.
    fn written_before(row: &Instance, intent: &TerminalHandoffIntent) -> Result<Value> {
        let mut value = serde_json::to_value(row)?;
        let mut handoff = serde_json::to_value(intent)?;
        handoff.as_object_mut().unwrap().remove("outcome");
        handoff["opaque_handoff"] = json!({"keep": 3});
        value["terminal_handoff"] = handoff;
        value["opaque"] = json!({"keep": 17});
        Ok(value)
    }

    #[test]
    fn a_handoff_without_an_outcome_records_the_one_its_target_gave() -> Result<()> {
        let mut row = Instance::new("pending", "/tmp/repo");
        row.agent_session_id = Some("sid-stored".into());
        let resuming = row.conversation_state();
        let mut cleared = resuming.clone();
        cleared.intent = ResumeIntent::Cleared;
        let keep = TerminalHandoffContext::Keep {
            native_store: crate::session::ExecutionBinding {
                agent: "claude".into(),
                stores: vec!["/tmp/claude-store".into()],
                configuration: Vec::new(),
                cwd: "/tmp".into(),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
                exported_default_store: None,
            },
        };
        let cases = [
            ("keep", keep, None, "continue"),
            (
                "discard, cleared",
                TerminalHandoffContext::Discard,
                Some(cleared),
                "fresh",
            ),
            (
                "discard, none",
                TerminalHandoffContext::Discard,
                None,
                "fresh",
            ),
            (
                "discard, resuming",
                TerminalHandoffContext::Discard,
                Some(resuming),
                "continue",
            ),
        ];
        let root = tempfile::tempdir()?;
        let path = root.path().join("sessions.json");
        let mut rows = vec![serde_json::to_value(Instance::new("plain", "/tmp/repo"))?];
        for (_, context, terminal, _) in &cases {
            let mut intent = pending(&row, context.clone());
            intent.terminal = terminal.clone();
            rows.push(written_before(&row, &intent)?);
        }
        std::fs::write(&path, serde_json::to_vec(&rows)?)?;

        migrate_file(&path)?;

        let migrated: Vec<Value> = serde_json::from_slice(&std::fs::read(&path)?)?;
        assert_eq!(migrated[0], rows[0]);
        for (index, (name, _, _, outcome)) in cases.iter().enumerate() {
            let (before, after) = (&rows[index + 1], &migrated[index + 1]);
            assert_eq!(after["terminal_handoff"]["outcome"], *outcome, "{name}");
            let mut unchanged = after.clone();
            unchanged["terminal_handoff"]
                .as_object_mut()
                .unwrap()
                .remove("outcome");
            assert_eq!(&unchanged, before, "{name}: only the outcome is added");
        }
        assert_eq!(crate::session::migration_backups(&path)?.len(), 1);
        let committed = std::fs::read(&path)?;
        migrate_file(&path)?;
        assert_eq!(std::fs::read(&path)?, committed);
        assert_eq!(crate::session::migration_backups(&path)?.len(), 1);
        Ok(())
    }

    #[test]
    fn unreadable_or_malformed_components_keep_their_bytes() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("sessions.json");
        for unreadable in [b"{".as_slice(), b"{}", &[0xff]] {
            std::fs::write(&path, unreadable)?;
            migrate_file(&path)?;
            assert_eq!(std::fs::read(&path)?, unreadable);
            assert!(crate::session::migration_backups(&path)?.is_empty());
        }
        let row = Instance::new("pending", "/tmp/repo");
        let intent = pending(&row, TerminalHandoffContext::Discard);
        let corruptions: [fn(&mut Value); 4] = [
            |row| row["terminal_handoff"] = json!("not a handoff"),
            |row| row["terminal_handoff"]["context"] = json!({"mode": "unknown"}),
            // Context and target are valid, so only the whole handoff shows these.
            |row| row["terminal_handoff"]["id"] = json!("not-a-uuid"),
            |row| row["terminal_handoff"]["target_generation"] = json!(1),
        ];
        for corrupt in corruptions {
            let mut value = written_before(&row, &intent)?;
            corrupt(&mut value);
            let bytes = serde_json::to_vec(&json!([value]))?;
            std::fs::write(&path, &bytes)?;
            assert!(migrate_file(&path).is_err());
            assert_eq!(std::fs::read(&path)?, bytes);
            assert!(crate::session::migration_backups(&path)?.is_empty());
        }
        // A handoff that already has an outcome is validated too, beside one that needs it.
        let recorded: [fn(&mut Value); 3] = [
            |row| row["terminal_handoff"]["outcome"] = json!("sideways"),
            |row| row["terminal_handoff"]["id"] = json!("not-a-uuid"),
            |row| row["terminal_handoff"]["target_generation"] = json!(1),
        ];
        for corrupt in recorded {
            let mut value = written_before(&row, &intent)?;
            value["terminal_handoff"]["outcome"] = json!("fresh");
            corrupt(&mut value);
            let bytes = serde_json::to_vec(&json!([written_before(&row, &intent)?, value]))?;
            std::fs::write(&path, &bytes)?;
            assert!(migrate_file(&path).is_err());
            assert_eq!(std::fs::read(&path)?, bytes);
            assert!(crate::session::migration_backups(&path)?.is_empty());
        }
        Ok(())
    }

    #[test]
    fn a_symlinked_sessions_file_is_refused_and_kept() -> Result<()> {
        let root = tempfile::tempdir()?;
        let target = root.path().join("elsewhere.json");
        let path = root.path().join("sessions.json");
        let row = Instance::new("pending", "/tmp/repo");
        let intent = pending(&row, TerminalHandoffContext::Discard);
        let bytes = serde_json::to_vec(&json!([written_before(&row, &intent)?]))?;
        std::fs::write(&target, &bytes)?;
        std::os::unix::fs::symlink(&target, &path)?;

        let error = migrate_file(&path).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("requires a regular sessions file"),
            "{error:#}"
        );
        assert_eq!(std::fs::read_link(&path)?, target);
        assert_eq!(std::fs::read(&target)?, bytes);
        assert!(crate::session::migration_backups(&path)?.is_empty());
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn a_handoff_written_before_the_migration_loads_as_it_was_read() -> Result<()> {
        // From 47, v048 validates the handoff before v060 records its outcome.
        for version in ["47", "58"] {
            let home = tempfile::tempdir()?;
            let _home = crate::session::test_support::isolate_app_dir_at(home.path());
            let app = crate::session::get_app_dir()?;
            let storage = crate::session::Storage::new_unwatched("default")?;
            let mut row = Instance::new("pending", "/tmp/repo");
            row.agent_session_id = Some("sid-stored".into());
            let mut intent = pending(&row, TerminalHandoffContext::Discard);
            intent.terminal = Some(row.conversation_state());
            let bytes = serde_json::to_vec(&json!([written_before(&row, &intent)?]))?;
            std::fs::write(storage.sessions_path(), &bytes)?;
            assert!(storage.load().is_err(), "the field is required");
            std::fs::write(app.join(".schema_version"), version)?;

            crate::migrations::run_migrations()?;

            assert_eq!(
                crate::migrations::get_current_version(),
                crate::migrations::CURRENT_VERSION
            );
            let loaded = storage.load()?;
            let handoff = loaded[0].terminal_handoff.as_deref().expect("kept handoff");
            assert_eq!(
                handoff.outcome,
                TerminalHandoffOutcome::of(&intent.context, intent.terminal.as_ref()),
                "from {version}"
            );
            assert_eq!(handoff.outcome, TerminalHandoffOutcome::Continue);
            assert_eq!(handoff.terminal, intent.terminal);
            assert_eq!(handoff.context, intent.context);
            assert_eq!(handoff.id, intent.id);
            assert_eq!(loaded[0].conversation_state(), row.conversation_state());
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn a_component_repaired_after_v060_gets_its_outcome_on_recovery() -> Result<()> {
        let home = tempfile::tempdir()?;
        let _home = crate::session::test_support::isolate_app_dir_at(home.path());
        let app = crate::session::get_app_dir()?;
        let storage = crate::session::Storage::new_unwatched("default")?;
        std::fs::write(storage.sessions_path(), b"{")?;
        std::fs::write(app.join(".schema_version"), "58")?;
        crate::migrations::run_migrations()?;
        assert_eq!(
            crate::migrations::get_current_version(),
            crate::migrations::CURRENT_VERSION
        );
        assert_eq!(std::fs::read(storage.sessions_path())?, b"{");

        let row = Instance::new("repaired", "/tmp/repo");
        let intent = pending(&row, TerminalHandoffContext::Discard);
        let repaired = serde_json::to_vec(&json!([written_before(&row, &intent)?]))?;
        std::fs::write(storage.sessions_path(), &repaired)?;
        assert!(storage.load().is_err(), "the repaired row has no outcome");

        crate::migrations::recovery::reconcile_repaired_components()?;

        let loaded = storage.load()?;
        let handoff = loaded[0].terminal_handoff.as_deref().expect("kept handoff");
        assert_eq!(handoff.outcome, TerminalHandoffOutcome::Fresh);
        assert_eq!(handoff.id, intent.id);
        Ok(())
    }
}
