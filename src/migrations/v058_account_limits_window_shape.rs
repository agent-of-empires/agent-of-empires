//! Rebuild `acp_account_limits` for databases created before it keyed rows by quota window.
//!
//! Early builds stored one row per account with no `window_kind` key and no `estimated`
//! column. The table only caches limit deadlines that expire on their own, so an outdated one is
//! dropped and `EventStore::open` recreates it in the current shape.

use anyhow::{Context, Result};
use std::path::Path;
use tracing::info;

const TABLE: &str = "acp_account_limits";

pub fn run() -> Result<()> {
    migrate_database(&crate::session::get_app_dir()?.join("acp_events.db"))
}

fn migrate_database(path: &Path) -> Result<()> {
    if !path.try_exists()? {
        return Ok(());
    }
    let conn = rusqlite::Connection::open(path)
        .with_context(|| format!("open event store {}", path.display()))?;
    reconcile_table(&conn)?;
    Ok(())
}

/// Drop an outdated cache and report whether a current table remains.
pub(super) fn reconcile_table(conn: &rusqlite::Connection) -> Result<bool> {
    let mut statement = conn.prepare("SELECT name, pk FROM pragma_table_info(?1)")?;
    let columns: Vec<(String, i64)> = statement
        .query_map([TABLE], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    drop(statement);
    if columns.is_empty() {
        return Ok(false);
    }
    if has_current_shape(&columns) {
        return Ok(true);
    }
    conn.execute_batch(&format!("DROP TABLE {TABLE};"))
        .with_context(|| format!("drop outdated {TABLE}"))?;
    info!(target: "migrations.v058", "Dropped the outdated account limits table; it is recreated on open");
    Ok(false)
}

fn has_current_shape(columns: &[(String, i64)]) -> bool {
    let primary_key = |name: &str| {
        columns
            .iter()
            .find(|(column, _)| column == name)
            .map(|(_, pk)| *pk)
    };
    [
        "limited_until_ms",
        "unusable_until_ms",
        "updated_at_ms",
        "estimated",
    ]
    .iter()
    .all(|name| primary_key(name) == Some(0))
        && primary_key("account_key") == Some(1)
        && primary_key("window_kind") == Some(2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::{event_store::EventStore, state::Event};

    /// The table as the early build that predates window keys created it.
    fn write_outdated_database(path: &Path) {
        let store = EventStore::open(path, 100, Default::default()).unwrap();
        store
            .record(
                "s-1",
                1,
                &Event::Stopped {
                    reason: "end_turn".into(),
                },
                &crate::acp::event_sender::EventMetadata::history(),
            )
            .unwrap();
        drop(store);
        rusqlite::Connection::open(path)
            .unwrap()
            .execute_batch(&format!(
                "DROP TABLE {TABLE};
                 CREATE TABLE {TABLE} (
                    account_key       TEXT    PRIMARY KEY,
                    limited_until_ms  INTEGER NOT NULL DEFAULT 0,
                    window_kind       TEXT    NOT NULL DEFAULT '',
                    unusable_until_ms INTEGER NOT NULL DEFAULT 0,
                    updated_at_ms     INTEGER NOT NULL
                 );
                 INSERT INTO {TABLE} VALUES ('/a', 9, 'usage', 0, 1);"
            ))
            .unwrap();
    }

    #[test]
    fn an_outdated_table_fails_to_open_then_works_after_the_migration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acp_events.db");
        write_outdated_database(&path);

        let broken = EventStore::open(&path, 100, Default::default()).unwrap();
        assert!(
            broken.account_state_checked("/a").is_err(),
            "without the migration every read fails"
        );
        broken.record_account_limit("/a", 10, "usage", 5, false);
        assert!(
            broken.account_state_checked("/a").is_err(),
            "and a write does not land"
        );
        drop(broken);

        migrate_database(&path).unwrap();
        let store = EventStore::open(&path, 100, Default::default()).unwrap();
        assert_eq!(
            store.highest_seq("s-1").unwrap(),
            1,
            "existing events survive"
        );
        assert_eq!(store.account_state_checked("/a").unwrap(), None);
        store.record_account_limit("/a", 10, "usage", 5, false);
        let state = store.account_state_checked("/a").unwrap().unwrap();
        assert_eq!(
            state.window("usage").map(|window| window.until_ms),
            Some(10)
        );
    }

    #[test]
    fn a_current_table_keeps_its_rows_and_a_second_run_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acp_events.db");
        let store = EventStore::open(&path, 100, Default::default()).unwrap();
        store.record_account_limit("/a", 10, "usage", 5, true);
        drop(store);

        migrate_database(&path).unwrap();
        migrate_database(&path).unwrap();
        let store = EventStore::open(&path, 100, Default::default()).unwrap();
        assert!(store.account_state_checked("/a").unwrap().is_some());
    }

    #[test]
    fn missing_database_and_missing_table_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acp_events.db");
        migrate_database(&path).unwrap();
        assert!(!path.exists());
        drop(rusqlite::Connection::open(&path).unwrap());
        migrate_database(&path).unwrap();
    }
}
