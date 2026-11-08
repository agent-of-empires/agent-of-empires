use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{
    backup::{Backup, StepResult},
    Connection, OpenFlags, TransactionBehavior,
};

use crate::acp::event_store::{EventStore, EventTableShape};

pub fn run() -> Result<()> {
    migrate(&crate::session::get_app_dir()?.join("acp_events.db"))
}

fn migrate(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("inspect ACP event database"),
        Ok(metadata) => anyhow::ensure!(
            metadata.is_file(),
            "ACP event database is not a regular file"
        ),
    }
    let mut conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .context("open ACP database for migration55")?;
    let shape = EventStore::event_table_shape(&conn)?;
    if shape == EventTableShape::Scoped {
        EventStore::validate_scope_rows(&conn)?;
        EventStore::validate_auxiliary_tables(&conn)?;
        return Ok(());
    }
    snapshot(&conn, path)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    match shape {
        EventTableShape::Absent => EventStore::create_scoped_event_table(&tx)?,
        EventTableShape::Legacy { discriminant } => {
            if !discriminant {
                tx.execute_batch("ALTER TABLE acp_events ADD COLUMN discriminant TEXT;")?;
            }
            tx.execute_batch(
                "UPDATE acp_events SET discriminant = substr(
                    event_json, instr(event_json, '\"') + 1,
                    instr(substr(event_json, instr(event_json, '\"') + 1), '\"') - 1
                 ) WHERE discriminant IS NULL AND instr(event_json, '\"') > 0;
                 ALTER TABLE acp_events ADD COLUMN scope INTEGER NOT NULL DEFAULT 0
                    CHECK(scope IN (0,1,2,3));
                 ALTER TABLE acp_events ADD COLUMN qualification_json TEXT
                    CHECK(scope IN (0,1) OR qualification_json IS NOT NULL);",
            )?;
        }
        EventTableShape::Scoped => unreachable!("scoped database returned before migration"),
    }
    let schema = crate::events::Schema::new("acp")?;
    crate::events::initialize_schema(&tx, &schema)?;
    // Freeze migration55's schema; publication metadata belongs to migration56.
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS acp_establishment_commit (
            session_id TEXT PRIMARY KEY NOT NULL, seal_json TEXT NOT NULL
         );",
    )?;
    EventStore::validate_scope_rows(&tx)?;
    EventStore::validate_auxiliary_tables(&tx)?;
    EventStore::preserve_legacy_budgets(&tx, &schema)?;
    tx.commit()?;
    tracing::info!(target: "migrations.v055", "ACP event scope migrated to55; historical rows remain unqualified");
    Ok(())
}

pub(super) fn snapshot(source: &Connection, path: &Path) -> Result<()> {
    let parent = path.parent().context("ACP database has no parent")?;
    let snapshot = tempfile::NamedTempFile::new_in(parent)?;
    let mut destination = Connection::open_with_flags(
        snapshot.path(),
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    destination.pragma_update(None, "journal_mode", "DELETE")?;
    {
        let backup = Backup::new(source, &mut destination)?;
        anyhow::ensure!(
            matches!(backup.step(-1)?, StepResult::Done),
            "ACP migration snapshot is busy or incomplete"
        );
    }
    destination
        .close()
        .map_err(|(_, error)| error)
        .context("close ACP migration snapshot")?;
    crate::session::publish_migration_snapshot(path, snapshot)
        .context("publish durable ACP migration snapshot")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn old_database(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA wal_autocheckpoint=0;
             CREATE TABLE acp_events (
                session_id TEXT NOT NULL, seq INTEGER NOT NULL,
                event_json TEXT NOT NULL, created_at INTEGER NOT NULL,
                PRIMARY KEY(session_id, seq)
             );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn wal_snapshot_preserves_history_attachments_and_budget_on_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("acp_events.db");
        let old = old_database(&path);
        for (seq, json) in [
            (1, r#"{"RateLimitAutoResumed":{"manual":false}}"#),
            (2, r#"{"UserPromptSent":{"text":"first"}}"#),
            (3, r#"{"RateLimitAutoResumed":{"manual":false}}"#),
            (4, r#"{"UserPromptSent":{"text":"second"}}"#),
            (
                5,
                r#" { "FutureDiagnostic" : {"detail":"preserve bytes"} } "#,
            ),
        ] {
            old.execute(
                "INSERT INTO acp_events VALUES ('history',?1,?2,17)",
                params![seq, json],
            )
            .unwrap();
        }
        old.execute_batch(
            "CREATE TABLE acp_attachments (
                session_id TEXT NOT NULL, seq INTEGER NOT NULL, attachment_id TEXT NOT NULL,
                kind TEXT NOT NULL, mime_type TEXT NOT NULL, name TEXT, data BLOB NOT NULL,
                created_at INTEGER NOT NULL, PRIMARY KEY(session_id, attachment_id)
             );
             INSERT INTO acp_attachments VALUES ('history',2,'image','image','image/png',NULL,x'001122',17);
             CREATE TABLE acp_rate_limit_budgets (
                session_id TEXT NOT NULL PRIMARY KEY, spent INTEGER NOT NULL, armed INTEGER NOT NULL
             );
             INSERT INTO acp_rate_limit_budgets VALUES ('pruned',7,1);"
        ).unwrap();
        let raw_path = temp.path().join("raw-copy.db");
        std::fs::copy(&path, &raw_path).unwrap();
        let raw = Connection::open(raw_path).unwrap();
        assert!(raw
            .query_row("SELECT COUNT(*) FROM acp_events", [], |row| row
                .get::<_, i64>(0))
            .is_err());
        migrate(&path).unwrap();
        let backup_path = crate::session::migration_backups(&path)
            .unwrap()
            .pop()
            .unwrap()
            .1;
        let backup = Connection::open(backup_path).unwrap();
        assert_eq!(
            backup
                .query_row("SELECT COUNT(*) FROM acp_events", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            5
        );
        assert_eq!(
            backup
                .query_row("SELECT data FROM acp_attachments", [], |row| row
                    .get::<_, Vec<u8>>(0))
                .unwrap(),
            [0, 17, 34]
        );
        assert_eq!(
            backup
                .query_row(
                    "SELECT spent,armed FROM acp_rate_limit_budgets WHERE session_id='pruned'",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                )
                .unwrap(),
            (7, 1)
        );
        assert_eq!(
            old.query_row(
                "SELECT spent,armed FROM acp_rate_limit_budgets WHERE session_id='pruned'",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            )
            .unwrap(),
            (7, 1)
        );
        // Unqualified historical retries cannot establish durable budget knowledge.
        assert_eq!(
            old.query_row(
                "SELECT COUNT(*) FROM acp_rate_limit_budgets WHERE session_id='history'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            old.query_row(
                "SELECT scope,qualification_json,event_json FROM acp_events WHERE seq=5",
                [],
                |row| Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?
                ))
            )
            .unwrap(),
            (
                0,
                None,
                r#" { "FutureDiagnostic" : {"detail":"preserve bytes"} } "#.to_string()
            )
        );
        migrate(&path).unwrap();
        assert_eq!(crate::session::migration_backups(&path).unwrap().len(), 1);
        drop(old);
        let reopened = Connection::open(&path).unwrap();
        EventStore::validate_scope_rows(&reopened).unwrap();
        EventStore::validate_auxiliary_tables(&reopened).unwrap();
        assert_eq!(
            reopened
                .query_row(
                    "SELECT spent,armed FROM acp_rate_limit_budgets WHERE session_id='pruned'",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                )
                .unwrap(),
            (7, 1)
        );
        assert_eq!(
            reopened
                .query_row(
                    "SELECT COUNT(*) FROM acp_rate_limit_budgets WHERE session_id='history'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        assert_eq!(
            reopened
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('acp_establishment_commit')
                     WHERE name='prompt_commit_json'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        assert_eq!(
            reopened
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('acp_rate_limit_budgets')
                     WHERE name='known'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn invalid_history_or_partial_scope_never_commits_migration() {
        for partial_scope in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("acp_events.db");
            let old = old_database(&path);
            old.execute("INSERT INTO acp_events VALUES ('broken',1,'{',17)", [])
                .unwrap();
            if partial_scope {
                old.execute_batch(
                    "ALTER TABLE acp_events ADD COLUMN scope INTEGER NOT NULL DEFAULT 0;",
                )
                .unwrap();
            }
            assert!(migrate(&path).is_err());
            let qualification_columns: i64 = old.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('acp_events') WHERE name='qualification_json'", [], |row| row.get(0),
            ).unwrap();
            assert_eq!(qualification_columns, 0);
            assert_eq!(
                old.query_row("SELECT event_json FROM acp_events", [], |row| row
                    .get::<_, String>(0))
                    .unwrap(),
                "{"
            );
        }
    }
}
