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
        Err(error) => return Err(error).context("inspect ACP budget database"),
        Ok(metadata) => anyhow::ensure!(metadata.is_file(), "ACP database is not a regular file"),
    }
    let mut conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .context("open ACP database for migration57")?;
    anyhow::ensure!(
        EventStore::event_table_shape(&conn)? == EventTableShape::Scoped,
        "ACP budget knowledge requires completed migration55"
    );
    EventStore::validate_auxiliary_tables(&conn)?;
    EventStore::validate_prompt_commit_schema(&conn)?;
    let invalid: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM acp_rate_limit_budgets
         WHERE typeof(spent) != 'integer' OR spent < 0
            OR typeof(armed) != 'integer' OR armed NOT IN (0,1))",
        [],
        |row| row.get(0),
    )?;
    anyhow::ensure!(!invalid, "invalid durable rate-limit budget");
    let present: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('acp_rate_limit_budgets')
         WHERE name = 'known')",
        [],
        |row| row.get(0),
    )?;
    if present {
        return EventStore::validate_budget_knowledge(&conn);
    }
    super::v055_acp_event_scope::snapshot(&conn, path)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "ALTER TABLE acp_rate_limit_budgets
         ADD COLUMN known INTEGER NOT NULL DEFAULT 1 CHECK (known IN (0,1));",
    )?;
    EventStore::validate_budget_knowledge(&tx)?;
    tx.commit()?;
    tracing::info!(target: "migrations.v057", "ACP budget knowledge migrated to57");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_preserves_budget_and_wal_history_without_restoring_unknown_knowledge() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("acp_events.db");
        let old = Connection::open(&path).unwrap();
        old.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
            .unwrap();
        EventStore::create_scoped_event_table(&old).unwrap();
        crate::events::initialize_schema(&old, &crate::events::Schema::new("acp").unwrap())
            .unwrap();
        old.execute_batch(
            "CREATE TABLE acp_establishment_commit (
                session_id TEXT PRIMARY KEY NOT NULL, seal_json TEXT NOT NULL,
                prompt_commit_json TEXT
             );
             INSERT INTO acp_rate_limit_budgets VALUES ('retained',4,1);",
        )
        .unwrap();
        let original = " { \"original_proof\" : [1, 2, 3] } ";
        let history = " { \"FutureDiagnostic\" : {\"text\":\"retained bytes\"} } ";
        old.execute(
            "INSERT INTO acp_establishment_commit VALUES ('retained',?1,NULL)",
            [original],
        )
        .unwrap();
        old.execute(
            "INSERT INTO acp_events VALUES ('retained',7,?1,17,'FutureDiagnostic',0,NULL)",
            [history],
        )
        .unwrap();
        old.execute(
            "INSERT INTO acp_events VALUES ('unbudgeted',1,?1,17,'FutureDiagnostic',0,NULL)",
            [history],
        )
        .unwrap();
        migrate(&path).unwrap();
        assert_eq!(
            old.query_row(
                "SELECT spent,armed,known FROM acp_rate_limit_budgets",
                [],
                |row| Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?
                )),
            )
            .unwrap(),
            (4, 1, 1)
        );
        assert_eq!(
            old.query_row(
                "SELECT COUNT(*) FROM acp_rate_limit_budgets WHERE session_id='unbudgeted'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            old.query_row(
                "SELECT event_json FROM acp_events WHERE session_id='retained'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            history
        );
        assert_eq!(
            old.query_row(
                "SELECT seal_json FROM acp_establishment_commit",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            original
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
                    "SELECT spent,armed FROM acp_rate_limit_budgets",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                )
                .unwrap(),
            (4, 1)
        );
        assert_eq!(
            backup
                .query_row(
                    "SELECT event_json FROM acp_events WHERE session_id='retained'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            history
        );
        assert_eq!(backup.query_row("SELECT COUNT(*) FROM pragma_table_info('acp_rate_limit_budgets') WHERE name='known'", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
        old.execute("UPDATE acp_rate_limit_budgets SET known=0", [])
            .unwrap();
        migrate(&path).unwrap();
        assert_eq!(
            old.query_row(
                "SELECT spent,armed,known FROM acp_rate_limit_budgets",
                [],
                |row| Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?
                ))
            )
            .unwrap(),
            (4, 1, 0)
        );
        assert_eq!(crate::session::migration_backups(&path).unwrap().len(), 1);
        EventStore::open(&path, 100).unwrap();
        let reopened = Connection::open(&path).unwrap();
        assert_eq!(
            reopened
                .query_row(
                    "SELECT COUNT(*) FROM acp_rate_limit_budgets WHERE session_id='unbudgeted'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }
}
