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

/// A linked `sessions.json` is read and rewritten at its target, as storage does, under the
/// profile directory's storage lock.
fn migrate_file(path: &Path) -> Result<()> {
    // Follows links, so a dangling link (missing leaf or directory) is skipped like v045.
    if !path.try_exists()? {
        return Ok(());
    }
    let not_regular = || {
        format!(
            "terminal handoff migration requires a regular sessions file: {}",
            path.display()
        )
    };
    let directory = path.parent().context("sessions file has no parent")?;
    let _lock =
        crate::session::acquire_storage_flock(directory, crate::session::STORAGE_LOCK_FILENAME)?;
    let file =
        crate::session::anchored_fs::ResolvedDataFile::open(path).with_context(not_regular)?;
    let Some((mut snapshot, _)) = file.open_read_snapshot().with_context(not_regular)? else {
        return Ok(());
    };
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut snapshot, &mut bytes)?;
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
    let mut handoffs = 0;
    for (index, row) in rows.iter_mut().enumerate() {
        let Some(intent) = row
            .get_mut("terminal_handoff")
            .filter(|value| !value.is_null())
        else {
            continue;
        };
        handoffs += 1;
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
        file.replace_preserving_permissions(serde_json::to_vec_pretty(&rows)?.as_slice())?;
        tracing::info!(target: "migrations.v060", path = %path.display(), recorded, "Recorded terminal handoff outcomes");
    }
    // The schema marker must not become durable before the rewrite, so the target's directory is
    // synced, as storage does after a replace. A rerun after a failed sync finds the rewrite
    // already visible and syncs it again.
    if handoffs > 0 {
        file.sync_parent()
            .context("terminal handoff outcomes were written, but their directory sync failed")?;
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
    fn a_linked_sessions_file_is_migrated_at_its_regular_target() -> Result<()> {
        let row = Instance::new("pending", "/tmp/repo");
        let intent = pending(&row, TerminalHandoffContext::Discard);
        let bytes = serde_json::to_vec(&json!([written_before(&row, &intent)?]))?;

        let root = tempfile::tempdir()?;
        let target = root.path().join("dotfiles").join("sessions.json");
        std::fs::create_dir(target.parent().unwrap())?;
        std::fs::write(&target, &bytes)?;
        let path = root.path().join("sessions.json");
        std::os::unix::fs::symlink(&target, &path)?;
        migrate_file(&path)?;
        assert_eq!(std::fs::read_link(&path)?, target, "the link is kept");
        let rows: Value = serde_json::from_slice(&std::fs::read(&target)?)?;
        assert_eq!(rows[0]["terminal_handoff"]["outcome"], "fresh");
        assert_eq!(crate::session::migration_backups(&path)?.len(), 1);
        let lock = crate::session::STORAGE_LOCK_FILENAME;
        assert!(
            root.path().join(lock).exists(),
            "locked where storage locks"
        );
        assert!(
            !target.with_file_name(lock).exists(),
            "not beside the target"
        );

        // A link to a directory is refused; a dangling one has no rows, so it is skipped.
        for (name, goal, refused) in [
            ("a directory", "elsewhere", true),
            ("a missing file", "missing.json", false),
            ("a missing directory", "absent/missing.json", false),
        ] {
            let root = tempfile::tempdir()?;
            let target = root.path().join(goal);
            if refused {
                std::fs::create_dir(&target)?;
            }
            let path = root.path().join("sessions.json");
            std::os::unix::fs::symlink(&target, &path)?;
            match migrate_file(&path) {
                Err(error) => assert!(
                    refused && format!("{error:#}").contains("requires a regular sessions file"),
                    "{name}: {error:#}"
                ),
                Ok(()) => assert!(!refused, "{name}: accepted"),
            }
            assert_eq!(std::fs::read_link(&path)?, target, "{name}");
            assert_eq!(target.exists(), refused, "{name}");
            assert!(
                crate::session::migration_backups(&path)?.is_empty(),
                "{name}"
            );
        }
        Ok(())
    }

    #[test]
    fn a_linked_targets_directory_is_synced_before_the_migration_counts() -> Result<()> {
        use crate::session::anchored_fs::FAIL_SYNC_ONCE;
        let _reset = crate::session::anchored_fs::ResetExchangeHooks;
        let row = Instance::new("pending", "/tmp/repo");
        let intent = pending(&row, TerminalHandoffContext::Discard);
        let root = tempfile::tempdir()?;
        let target = root.path().join("dotfiles").join("sessions.json");
        std::fs::create_dir(target.parent().unwrap())?;
        std::fs::write(
            &target,
            serde_json::to_vec(&json!([written_before(&row, &intent)?]))?,
        )?;
        let path = root.path().join("sessions.json");
        std::os::unix::fs::symlink(&target, &path)?;
        let outcome = || -> Result<Value> {
            let rows: Value = serde_json::from_slice(&std::fs::read(&target)?)?;
            Ok(rows[0]["terminal_handoff"]["outcome"].clone())
        };

        // A failed sync of the target's directory fails the migration after the rewrite, and so
        // does the rerun, which finds nothing left to record but still syncs.
        for attempt in ["first run", "rerun"] {
            FAIL_SYNC_ONCE.set(Some(target.parent().unwrap().to_path_buf()));
            let error = migrate_file(&path).unwrap_err();
            assert!(
                format!("{error:#}").contains("directory sync failed"),
                "{attempt}: {error:#}"
            );
            assert!(
                FAIL_SYNC_ONCE.with_borrow(Option::is_none),
                "{attempt}: synced"
            );
            assert_eq!(outcome()?, "fresh", "{attempt}");
        }
        // The link's own directory isn't the one synced.
        FAIL_SYNC_ONCE.set(Some(root.path().to_path_buf()));
        migrate_file(&path)?;
        assert!(FAIL_SYNC_ONCE.with_borrow(Option::is_some));
        assert_eq!(crate::session::migration_backups(&path)?.len(), 1);
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
    fn an_upgrade_from_before_v046_records_the_outcome_after_v059() -> Result<()> {
        let home = tempfile::tempdir()?;
        let _home = crate::session::test_support::isolate_app_dir_at(home.path());
        let app = crate::session::get_app_dir()?;
        let storage = crate::session::Storage::new_unwatched("default")?;
        let row = Instance::new("pending", "/tmp/repo");
        let intent = pending(&row, TerminalHandoffContext::Discard);
        let bytes = serde_json::to_vec(&json!([written_before(&row, &intent)?]))?;
        std::fs::write(storage.sessions_path(), &bytes)?;
        std::fs::write(app.join(".schema_version"), "45")?;
        // Whether the handoff has its outcome as each migration finishes.
        let finished = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (seen, path) = (finished.clone(), storage.sessions_path().to_path_buf());
        let reporter: crate::migrations::progress::Reporter = std::sync::Arc::new(move |event| {
            if let crate::migrations::progress::Event::Finished { version, .. } = event {
                let rows: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                let recorded = rows[0]["terminal_handoff"].get("outcome").is_some();
                seen.lock().unwrap().push((version, recorded));
            }
        });

        crate::migrations::run_migrations_with(Some(reporter))?;

        let finished = finished.lock().unwrap().clone();
        let expected: Vec<_> = crate::migrations::MIGRATIONS
            .iter()
            .map(|(version, ..)| *version)
            .filter(|version| *version > 45)
            .map(|version| (version, version >= 60))
            .collect();
        assert_eq!(finished, expected);
        let loaded = storage.load()?;
        let handoff = loaded[0].terminal_handoff.as_deref().expect("kept handoff");
        assert_eq!(handoff.outcome, TerminalHandoffOutcome::Fresh);
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
        crate::session::purge_owners::initialize(&app)?;

        crate::migrations::run_migrations_announced(None)?;

        let loaded = storage.load()?;
        let handoff = loaded[0].terminal_handoff.as_deref().expect("kept handoff");
        assert_eq!(handoff.outcome, TerminalHandoffOutcome::Fresh);
        assert_eq!(handoff.id, intent.id);
        Ok(())
    }
}
