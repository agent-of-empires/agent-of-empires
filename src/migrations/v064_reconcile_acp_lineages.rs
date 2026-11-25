//! Reconcile the published ACP54–57 and main54–55 data histories.

use anyhow::Result;

use super::v055_acp_event_scope::{resolve_database_path, snapshot};

pub fn run() -> Result<()> {
    super::v054_acp_establishment::run()?;
    super::v055_acp_event_scope::run()?;
    super::v056_native_prompt_publication::run()?;
    super::v057_acp_budget_knowledge::run()?;
    super::v054_retained_history_identity::run()?;
    super::v055_global_max_concurrent_workers::run()?;
    super::v056_launch_resolved_executable::run()?;
    super::v057_worker_launch_executable::run()?;
    super::v058_account_limits_window_shape::run()?;
    reconcile_autonomous_metadata()?;
    super::v059_owner_only_event_logs::run()?;
    super::v063_retire_color_labels::run()?;
    tracing::info!(target: "migrations.v064", "Reconciled ACP and main data histories");
    Ok(())
}

pub(super) fn autonomous_pending() -> Result<bool> {
    let path = crate::session::get_app_dir()?.join("acp_events.db");
    let Some(path) = resolve_database_path(&path)? else {
        return Ok(false);
    };
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    Ok(!conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('acp_establishment_commit')
         WHERE name='autonomous_commit_json')",
        [],
        |row| row.get::<_, bool>(0),
    )?)
}

pub(super) fn reconcile_autonomous_metadata() -> Result<()> {
    use crate::acp::event_store::{EventStore, EventTableShape};
    let path = crate::session::get_app_dir()?.join("acp_events.db");
    let Some(path) = resolve_database_path(&path)? else {
        return Ok(());
    };
    let mut conn = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    anyhow::ensure!(
        EventStore::event_table_shape(&conn)? == EventTableShape::Scoped,
        "Native lifecycle reconciliation requires scoped ACP history"
    );
    EventStore::validate_auxiliary_tables(&conn)?;
    EventStore::validate_prompt_commit_schema(&conn)?;
    EventStore::validate_budget_knowledge(&conn)?;
    let present: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('acp_establishment_commit')
         WHERE name='autonomous_commit_json')",
        [],
        |row| row.get(0),
    )?;
    if present {
        return EventStore::validate_autonomous_commit_schema(&conn);
    }
    snapshot(&conn, &path)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "ALTER TABLE acp_establishment_commit ADD COLUMN autonomous_commit_json TEXT;",
    )?;
    EventStore::validate_autonomous_commit_schema(&tx)?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use rusqlite::{params, Connection, OptionalExtension};
    use std::{fs, path::Path};

    const SID: &str = "5959595959595959";
    const OPAQUE_EVENT: &str = r#" { "FutureDiagnostic" : {"detail":"preserve bytes"} } "#;
    const OPAQUE_SEAL: &str = r#" { "uninterpreted_original_proof" : [1, 2, 3] } "#;
    const SESSION_BYTES: &[u8] = br#" [{"id":"5959595959595959","agent_session_id":"retained-native-history","acp_session_id":"retained-sdk-alias","created_at":"2026-01-01T00:00:00Z"}] "#;

    #[derive(Clone, Copy, Debug)]
    enum History {
        Legacy53,
        Main54,
        Main55,
        Main56,
        Main57,
        Acp54,
        Acp55,
        Acp56,
        Acp57,
        Frontier58,
    }

    fn seed_profiles(app: &Path) {
        for (name, contents) in [
            (
                "config.toml",
                "default_profile = 'work'\n[acp]\nmax_concurrent_workers = 100\n",
            ),
            (
                "profiles/work/config.toml",
                "[acp]\nmax_concurrent_workers = 3\nreplay_events = 9\n[theme]\nname = 'dracula'\n",
            ),
            (
                "profiles/other/config.toml",
                "[acp]\nmax_concurrent_workers = 1\n",
            ),
        ] {
            let path = app.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        fs::write(app.join("serve.rollback.launch"), br#"{"profile":"other"}"#).unwrap();
        fs::write(app.join("sessions.json"), SESSION_BYTES).unwrap();
    }

    fn seed_legacy_database(app: &Path) -> Connection {
        let conn = Connection::open(app.join("acp_events.db")).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA wal_autocheckpoint=0;
             CREATE TABLE acp_events (
                 session_id TEXT NOT NULL, seq INTEGER NOT NULL,
                 event_json TEXT NOT NULL, created_at INTEGER NOT NULL,
                 PRIMARY KEY(session_id,seq)
             );
             CREATE TABLE acp_attachments (
                 session_id TEXT NOT NULL, seq INTEGER NOT NULL, attachment_id TEXT NOT NULL,
                 kind TEXT NOT NULL, mime_type TEXT NOT NULL, name TEXT, data BLOB NOT NULL,
                 created_at INTEGER NOT NULL, PRIMARY KEY(session_id,attachment_id)
             );
             CREATE TABLE acp_rate_limit_budgets (
                 session_id TEXT NOT NULL PRIMARY KEY, spent INTEGER NOT NULL, armed INTEGER NOT NULL
             );
             INSERT INTO acp_rate_limit_budgets VALUES ('pruned',7,1);",
        ).unwrap();
        for (seq, event) in [
            (1, r#"{"RateLimitAutoResumed":{"manual":false}}"#),
            (2, r#"{"UserPromptSent":{"text":"retained prompt"}}"#),
            (3, OPAQUE_EVENT),
        ] {
            conn.execute(
                "INSERT INTO acp_events VALUES (?1,?2,?3,17)",
                params![SID, seq, event],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO acp_attachments VALUES (?1,2,'image','image','image/png',NULL,x'001122',17)",
            [SID],
        ).unwrap();
        conn
    }

    fn seed_frontier_58(conn: &Connection) {
        // This is the published v058 schema and stored Open lifecycle, not a new migration.
        conn.execute_batch(
            "CREATE TABLE acp_transcript_frontier (
                 session_id TEXT PRIMARY KEY NOT NULL,
                 version INTEGER NOT NULL CHECK(version=1),
                 through_seq INTEGER NOT NULL CHECK(through_seq>=0),
                 context_json TEXT NOT NULL
             );
             CREATE TABLE acp_transcript_tools (
                 session_id TEXT NOT NULL, tool_call_id TEXT NOT NULL,
                 phase INTEGER NOT NULL CHECK(phase IN (0,1,2)),
                 order_seq INTEGER NOT NULL CHECK(order_seq>=0),
                 phase_seq INTEGER NOT NULL CHECK(phase_seq>=0),
                 phase_scope INTEGER NOT NULL CHECK(phase_scope IN (0,1,2,3)),
                 phase_qualification_json TEXT,
                 output_scope INTEGER CHECK(output_scope IN (0,1,2,3)),
                 output_qualification_json TEXT,
                 elicitation INTEGER NOT NULL CHECK(elicitation IN (0,1)),
                 state_json TEXT NOT NULL,
                 PRIMARY KEY(session_id,tool_call_id)
             );
             CREATE INDEX acp_transcript_tools_phase_order
                 ON acp_transcript_tools(session_id,phase,order_seq,tool_call_id);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO acp_transcript_frontier VALUES (?1,1,3,?2)",
            params![SID, r#"{"last_message_id":null,"last_thought_id":null}"#],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO acp_transcript_tools VALUES (?1,'old-open',1,2,2,0,NULL,NULL,NULL,0,?2)",
            params![SID, r#"{"Open":{"first_seq":null,"text":""}}"#],
        )
        .unwrap();
    }

    fn prepare_history(history: History, conn: &Connection) {
        match history {
            History::Main54 | History::Main55 | History::Main56 | History::Main57 => {
                v054_retained_history_identity::run().unwrap();
            }
            History::Acp54
            | History::Acp55
            | History::Acp56
            | History::Acp57
            | History::Frontier58 => {
                v054_acp_establishment::run().unwrap();
            }
            History::Legacy53 => {}
        }
        if matches!(history, History::Main55 | History::Main56 | History::Main57) {
            v055_global_max_concurrent_workers::run().unwrap();
        }
        if matches!(history, History::Main56 | History::Main57) {
            v056_launch_resolved_executable::run().unwrap();
        }
        if matches!(history, History::Main57) {
            v057_worker_launch_executable::run().unwrap();
        }
        if matches!(
            history,
            History::Acp55 | History::Acp56 | History::Acp57 | History::Frontier58
        ) {
            v055_acp_event_scope::run().unwrap();
            conn.execute(
                "INSERT INTO acp_establishment_commit(session_id,seal_json) VALUES (?1,?2)",
                params![SID, OPAQUE_SEAL],
            )
            .unwrap();
        }
        if matches!(
            history,
            History::Acp56 | History::Acp57 | History::Frontier58
        ) {
            v056_native_prompt_publication::run().unwrap();
        }
        if matches!(history, History::Acp57 | History::Frontier58) {
            v057_acp_budget_knowledge::run().unwrap();
        }
        if matches!(history, History::Frontier58) {
            seed_frontier_58(conn);
        }
        let version = match history {
            History::Legacy53 => 53,
            History::Main54 | History::Acp54 => 54,
            History::Main55 | History::Acp55 => 55,
            History::Main56 | History::Acp56 => 56,
            History::Main57 | History::Acp57 => 57,
            History::Frontier58 => 58,
        };
        set_version(version).unwrap();
    }

    fn identity(conn: &Connection) -> Option<(String, String)> {
        let installed: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='acp_history_store')",
            [], |row| row.get(0),
        ).unwrap();
        installed.then(|| {
            conn.query_row(
                "SELECT incarnation,version FROM acp_history_store,acp_history_versions WHERE id=1 AND session_id=?1",
                [SID], |row| Ok((row.get(0)?,row.get(1)?)),
            ).optional().unwrap().unwrap()
        })
    }

    fn assert_preserved(app: &Path, conn: &Connection) {
        assert_eq!(fs::read(app.join("sessions.json")).unwrap(), SESSION_BYTES);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM acp_events", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            3
        );
        assert_eq!(
            conn.query_row("SELECT event_json FROM acp_events WHERE seq=3", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
            OPAQUE_EVENT
        );
        assert_eq!(
            conn.query_row("SELECT data FROM acp_attachments", [], |row| row
                .get::<_, Vec<u8>>(0))
                .unwrap(),
            [0, 17, 34]
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM acp_events WHERE scope!=0 OR qualification_json IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row(
                "SELECT spent,armed,known FROM acp_rate_limit_budgets WHERE session_id='pruned'",
                [],
                |row| Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?
                ))
            )
            .unwrap(),
            (7, 1, 1)
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM acp_rate_limit_budgets WHERE session_id=?1",
                [SID],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        let config: toml::Table = fs::read_to_string(app.join("config.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            config["acp"]["max_concurrent_workers"].as_integer(),
            Some(1)
        );
        let profile: toml::Table = fs::read_to_string(app.join("profiles/work/config.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(profile["acp"]["replay_events"].as_integer(), Some(9));
        assert_eq!(profile["theme"]["name"].as_str(), Some("dracula"));
        assert!(profile["acp"].get("max_concurrent_workers").is_none());
    }

    #[test]
    #[serial_test::serial]
    fn fresh_storage_reaches_current_version_without_manufacturing_native_history() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(home.path());
        let app = crate::session::get_app_dir().unwrap();
        fs::create_dir_all(&app).unwrap();
        run_migrations().unwrap();
        assert_eq!(get_current_version(), 64);
        assert!(!app.join("acp_events.db").exists());
    }

    #[test]
    #[serial_test::serial]
    fn reconciliation_preserves_published_histories_wal_data_and_retained_identity() {
        for history in [
            History::Legacy53,
            History::Main54,
            History::Main55,
            History::Main56,
            History::Main57,
            History::Acp54,
            History::Acp55,
            History::Acp56,
            History::Acp57,
            History::Frontier58,
        ] {
            let home = tempfile::tempdir().unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(home.path());
            let app = crate::session::get_app_dir().unwrap();
            fs::create_dir_all(&app).unwrap();
            seed_profiles(&app);
            let conn = seed_legacy_database(&app);
            prepare_history(history, &conn);
            let old_identity = identity(&conn);
            run_migrations().unwrap();
            assert_eq!(get_current_version(), 64, "{history:?}");
            assert_preserved(&app, &conn);
            let reconciled_identity = identity(&conn).unwrap();
            if let Some(old) = old_identity {
                assert_eq!(
                    reconciled_identity, old,
                    "scope reconciliation must not rotate retained content identity: {history:?}"
                );
            }
            if matches!(
                history,
                History::Acp55 | History::Acp56 | History::Acp57 | History::Frontier58
            ) {
                assert_eq!(
                    conn.query_row(
                        "SELECT seal_json FROM acp_establishment_commit WHERE session_id=?1",
                        [SID],
                        |row| row.get::<_, String>(0)
                    )
                    .unwrap(),
                    OPAQUE_SEAL
                );
            }
            if matches!(history, History::Frontier58) {
                assert_eq!(conn.query_row("SELECT version,through_seq,context_json FROM acp_transcript_frontier WHERE session_id=?1", [SID], |row| Ok((row.get::<_, i64>(0)?,row.get::<_, i64>(1)?,row.get::<_, String>(2)?))).unwrap(), (1,3,r#"{"last_message_id":null,"last_thought_id":null}"#.to_owned()));
                assert_eq!(conn.query_row("SELECT phase,phase_scope,state_json FROM acp_transcript_tools WHERE session_id=?1 AND tool_call_id='old-open'", [SID], |row| Ok((row.get::<_, i64>(0)?,row.get::<_, i64>(1)?,row.get::<_, String>(2)?))).unwrap(), (1,0,r#"{"Open":{"first_seq":null,"text":""}}"#.to_owned()));
            }
            let path = app.join("acp_events.db");
            let backups = crate::session::migration_backups(&path).unwrap();
            for (_, backup) in &backups {
                let snapshot = Connection::open(backup).unwrap();
                assert_eq!(
                    snapshot
                        .query_row("SELECT event_json FROM acp_events WHERE seq=3", [], |row| {
                            row.get::<_, String>(0)
                        })
                        .unwrap(),
                    OPAQUE_EVENT
                );
                assert_eq!(
                    snapshot
                        .query_row("SELECT data FROM acp_attachments", [], |row| row
                            .get::<_, Vec<u8>>(0))
                        .unwrap(),
                    [0, 17, 34]
                );
            }
            super::run().unwrap();
            run_migrations().unwrap();
            assert_preserved(&app, &conn);
            assert_eq!(identity(&conn).unwrap(), reconciled_identity);
            assert_eq!(
                crate::session::migration_backups(&path).unwrap().len(),
                backups.len()
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn a_late_config_failure_preserves_the_old_marker_and_reenters_completed_schema_steps() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(home.path());
        let app = crate::session::get_app_dir().unwrap();
        fs::create_dir_all(&app).unwrap();
        seed_profiles(&app);
        let conn = seed_legacy_database(&app);
        prepare_history(History::Main57, &conn);
        let old_identity = identity(&conn).unwrap();
        set_version(63).unwrap();
        let marker_before_reconciliation = fs::read(app.join(VERSION_FILE)).unwrap();
        let other = app.join("profiles/other/config.toml");
        let valid_config = fs::read(&other).unwrap();
        fs::write(&other, "[acp\n").unwrap();
        assert!(run_migrations().is_err());
        assert_eq!(
            fs::read(app.join(VERSION_FILE)).unwrap(),
            marker_before_reconciliation
        );
        assert_eq!(identity(&conn).unwrap(), old_identity);
        assert_eq!(
            conn.query_row(
                "SELECT known FROM acp_rate_limit_budgets WHERE session_id='pruned'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("SELECT event_json FROM acp_events WHERE seq=3", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
            OPAQUE_EVENT
        );
        let path = app.join("acp_events.db");
        let backups = crate::session::migration_backups(&path).unwrap().len();
        fs::write(&other, valid_config).unwrap();
        run_migrations().unwrap();
        assert_eq!(get_current_version(), 64);
        assert_preserved(&app, &conn);
        assert_eq!(identity(&conn).unwrap(), old_identity);
        assert_eq!(
            crate::session::migration_backups(&path).unwrap().len(),
            backups
        );
    }

    #[test]
    #[serial_test::serial]
    fn version_63_reconciles_missing_color_cleanup_or_missing_acp_schema() {
        for main_lineage in [false, true] {
            let _home = crate::session::test_support::isolate_app_dir();
            let app = crate::session::get_app_dir().unwrap();
            seed_profiles(&app);
            let config = app.join("config.toml");
            let mut contents = fs::read_to_string(&config).unwrap();
            contents.push_str("\n[session]\nshow_session_colors = true\n");
            fs::write(&config, contents).unwrap();
            let conn = seed_legacy_database(&app);
            prepare_history(History::Main57, &conn);
            if main_lineage {
                super::super::v063_retire_color_labels::run().unwrap();
            } else {
                super::run().unwrap();
                let mut contents = fs::read_to_string(&config).unwrap();
                contents.push_str("\n[session]\nshow_session_colors = true\n");
                fs::write(&config, contents).unwrap();
            }
            set_version(63).unwrap();

            run_migrations().unwrap();
            assert_eq!(get_current_version(), 64);
            assert_preserved(&app, &conn);
            let cleaned = fs::read_to_string(&config).unwrap();
            assert!(!cleaned.contains("show_session_colors"));
            run_migrations().unwrap();
            assert_eq!(fs::read_to_string(&config).unwrap(), cleaned);
        }
    }

    #[test]
    #[serial_test::serial]
    fn incompatible_legacy_scope_or_budget_preserves_data_without_stamping_reconciliation() {
        for malformed_scope in [true, false] {
            let home = tempfile::tempdir().unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(home.path());
            let app = crate::session::get_app_dir().unwrap();
            fs::create_dir_all(&app).unwrap();
            seed_profiles(&app);
            let conn = seed_legacy_database(&app);
            prepare_history(History::Main57, &conn);
            if malformed_scope {
                conn.execute_batch("ALTER TABLE acp_events ADD COLUMN scope TEXT")
                    .unwrap();
            } else {
                conn.execute(
                    "UPDATE acp_rate_limit_budgets SET spent=-1 WHERE session_id='pruned'",
                    [],
                )
                .unwrap();
            }
            set_version(63).unwrap();
            let marker = fs::read(app.join(VERSION_FILE)).unwrap();
            assert!(run_migrations().is_err());
            assert_eq!(fs::read(app.join(VERSION_FILE)).unwrap(), marker);
            assert_eq!(fs::read(app.join("sessions.json")).unwrap(), SESSION_BYTES);
            assert_eq!(
                conn.query_row("SELECT event_json FROM acp_events WHERE seq=3", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
                OPAQUE_EVENT
            );
            assert_eq!(
                conn.query_row("SELECT data FROM acp_attachments", [], |row| row
                    .get::<_, Vec<u8>>(0))
                    .unwrap(),
                [0, 17, 34]
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn malformed_readable_native_establishment_preserves_evidence_and_refuses_reconciliation() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(home.path());
        let app = crate::session::get_app_dir().unwrap();
        fs::create_dir_all(&app).unwrap();
        seed_profiles(&app);
        let conn = seed_legacy_database(&app);
        prepare_history(History::Main57, &conn);
        let malformed = br#"[{"id":"5959595959595959","acp_establishment":{"version":999}}]"#;
        fs::write(app.join("sessions.json"), malformed).unwrap();
        set_version(63).unwrap();
        let marker = fs::read(app.join(VERSION_FILE)).unwrap();
        assert!(run_migrations().is_err());
        assert_eq!(fs::read(app.join(VERSION_FILE)).unwrap(), marker);
        assert_eq!(fs::read(app.join("sessions.json")).unwrap(), malformed);
        assert_eq!(
            conn.query_row("SELECT event_json FROM acp_events WHERE seq=3", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
            OPAQUE_EVENT
        );
    }
}
