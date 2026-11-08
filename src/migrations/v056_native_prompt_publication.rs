use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, TransactionBehavior};

use crate::acp::event_store::{EventStore, EventTableShape};

pub fn run() -> Result<()> {
    migrate(&crate::session::get_app_dir()?.join("acp_events.db"))
}

fn migrate(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("inspect ACP publication database"),
        Ok(metadata) => anyhow::ensure!(metadata.is_file(), "ACP database is not a regular file"),
    }
    let mut conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .context("open ACP database for migration56")?;
    anyhow::ensure!(
        EventStore::event_table_shape(&conn)? == EventTableShape::Scoped,
        "ACP publication metadata requires completed migration55"
    );
    EventStore::validate_auxiliary_tables(&conn)?;
    let present: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('acp_establishment_commit')
         WHERE name = 'prompt_commit_json')",
        [],
        |row| row.get(0),
    )?;
    if present {
        return EventStore::validate_prompt_commit_schema(&conn);
    }
    super::v055_acp_event_scope::snapshot(&conn, path)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch("ALTER TABLE acp_establishment_commit ADD COLUMN prompt_commit_json TEXT;")?;
    EventStore::validate_prompt_commit_schema(&tx)?;
    tx.commit()?;
    tracing::info!(target: "migrations.v056", "Native prompt publication metadata migrated to56");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGINAL: &str = " { \"uninterpreted_original_proof\" : [1, 2, 3] } ";
    const HISTORY: &str = " { \"FutureDiagnostic\" : {\"text\":\"retained bytes\"} } ";

    fn old_database(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
            .unwrap();
        EventStore::create_scoped_event_table(&conn).unwrap();
        crate::events::initialize_schema(&conn, &crate::events::Schema::new("acp").unwrap())
            .unwrap();
        conn.execute_batch(
            "CREATE TABLE acp_establishment_commit (
                session_id TEXT PRIMARY KEY NOT NULL, seal_json TEXT NOT NULL
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO acp_establishment_commit VALUES ('history', ?1)",
            [ORIGINAL],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO acp_events
             (session_id,seq,event_json,created_at,discriminant,scope,qualification_json)
             VALUES ('history',7,?1,17,'FutureDiagnostic',0,NULL)",
            [HISTORY],
        )
        .unwrap();
        conn
    }

    #[test]
    fn upgrade_preserves_original_proof_history_and_wal_backup_without_inventing_publication() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("acp_events.db");
        let old = old_database(&path);
        migrate(&path).unwrap();
        let row: (String, Option<String>) = old
            .query_row(
                "SELECT seal_json,prompt_commit_json FROM acp_establishment_commit",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(row, (ORIGINAL.into(), None));
        assert_eq!(
            old.query_row("SELECT event_json FROM acp_events", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            HISTORY
        );
        let backup_path = crate::session::migration_backups(&path)
            .unwrap()
            .pop()
            .unwrap()
            .1;
        let backup = Connection::open(backup_path).unwrap();
        assert_eq!(
            backup
                .query_row(
                    "SELECT seal_json FROM acp_establishment_commit",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            ORIGINAL
        );
        assert_eq!(
            backup
                .query_row("SELECT event_json FROM acp_events", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            HISTORY
        );
        let columns: i64 = backup
            .query_row(
                "SELECT count(*) FROM pragma_table_info('acp_establishment_commit')
             WHERE name='prompt_commit_json'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(columns, 0, "backup retains the actual pre-migration schema");
        migrate(&path).unwrap();
        assert_eq!(crate::session::migration_backups(&path).unwrap().len(), 1);
        drop(old);
        let reopened = Connection::open(&path).unwrap();
        EventStore::validate_scope_rows(&reopened).unwrap();
        EventStore::validate_auxiliary_tables(&reopened).unwrap();
        EventStore::validate_prompt_commit_schema(&reopened).unwrap();
        assert_eq!(
            reopened
                .query_row(
                    "SELECT seal_json,prompt_commit_json FROM acp_establishment_commit",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                )
                .unwrap(),
            (ORIGINAL.into(), None)
        );
        assert_eq!(
            reopened
                .query_row("SELECT event_json FROM acp_events", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            HISTORY
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
    fn incompatible_publication_column_remains_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("acp_events.db");
        let old = old_database(&path);
        old.execute_batch(
            "ALTER TABLE acp_establishment_commit ADD COLUMN prompt_commit_json INTEGER;",
        )
        .unwrap();
        assert!(migrate(&path).is_err());
        assert_eq!(
            old.query_row(
                "SELECT seal_json FROM acp_establishment_commit",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            ORIGINAL
        );
        assert_eq!(old.query_row("SELECT type FROM pragma_table_info('acp_establishment_commit') WHERE name='prompt_commit_json'", [], |row| row.get::<_, String>(0)).unwrap(), "INTEGER");
        assert!(crate::session::migration_backups(&path).unwrap().is_empty());
    }
}
