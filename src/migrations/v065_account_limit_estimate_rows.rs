//! Move estimated account-limit deadlines to their own rows in `acp_account_limits`.
//!
//! Earlier builds kept an estimate under its window's own name, where it shared a row (and its
//! timestamp) with the window's reported deadline and rejected an older report. Estimates now
//! live under `<window>#estimate`. Where that row already holds an estimate, the later
//! observation stands, by the stamps and not by which build wrote it.

use super::{v055_acp_event_scope::resolve_database_path, v058_account_limits_window_shape};
use anyhow::{Context, Result};
use std::path::Path;
use tracing::info;

const TABLE: &str = "acp_account_limits";
const LOGIN_ROW: &str = "";
const ESTIMATE_SUFFIX: &str = "#estimate";

pub fn run() -> Result<()> {
    migrate_database(&crate::session::get_app_dir()?.join("acp_events.db"))
}

fn migrate_database(path: &Path) -> Result<()> {
    let Some(path) = resolve_database_path(path)? else {
        return Ok(());
    };
    let conn = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .with_context(|| format!("open event store {}", path.display()))?;
    // An ACP lineage marker may have skipped main's v058 window-key migration.
    if !v058_account_limits_window_shape::reconcile_table(&conn)? {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    let moved = tx
        .execute(
            &format!(
                "INSERT INTO {TABLE}
                    (account_key, window_kind, limited_until_ms, updated_at_ms, estimated)
                 SELECT account_key, window_kind || '{ESTIMATE_SUFFIX}', limited_until_ms,
                        updated_at_ms, 1
                   FROM {TABLE}
                  WHERE estimated = 1 AND window_kind != '{LOGIN_ROW}'
                    AND window_kind NOT LIKE '%{ESTIMATE_SUFFIX}'
                 ON CONFLICT(account_key, window_kind) DO UPDATE SET
                    limited_until_ms = excluded.limited_until_ms,
                    updated_at_ms = excluded.updated_at_ms
                  WHERE excluded.updated_at_ms > {TABLE}.updated_at_ms"
            ),
            [],
        )
        .with_context(|| format!("copy legacy estimates in {TABLE}"))?;
    tx.execute(
        &format!(
            "DELETE FROM {TABLE}
              WHERE estimated = 1 AND window_kind != '{LOGIN_ROW}'
                AND window_kind NOT LIKE '%{ESTIMATE_SUFFIX}'"
        ),
        [],
    )
    .with_context(|| format!("remove legacy estimates from {TABLE}"))?;
    tx.commit()?;
    if moved > 0 {
        info!(target: "migrations.v065", "Moved {moved} account-limit estimate(s) to their own rows");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::event_store::EventStore;

    const HOUR: i64 = 3_600_000;

    fn insert(path: &Path, kind: &str, until: i64, at: i64, estimated: bool) {
        rusqlite::Connection::open(path)
            .unwrap()
            .execute(
                &format!(
                    "INSERT INTO {TABLE}
                        (account_key, window_kind, limited_until_ms, updated_at_ms, estimated)
                     VALUES ('/a', ?1, ?2, ?3, ?4)"
                ),
                rusqlite::params![kind, until, at, estimated],
            )
            .unwrap();
    }

    fn rows(path: &Path) -> Vec<(String, i64, i64, bool)> {
        let conn = rusqlite::Connection::open(path).unwrap();
        let mut statement = conn
            .prepare(&format!(
                "SELECT window_kind, limited_until_ms, updated_at_ms, estimated
                   FROM {TABLE} ORDER BY window_kind"
            ))
            .unwrap();
        statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn database() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acp_events.db");
        drop(EventStore::open(&path, 100, Default::default()).unwrap());
        (dir, path)
    }

    #[test]
    fn a_legacy_estimate_moves_and_everything_else_stays() {
        let (_dir, path) = database();
        insert(&path, "weekly", 5 * HOUR, 100, true);
        insert(&path, "five_hour", 2 * HOUR, 90, false);
        insert(&path, "", 0, 0, false);
        insert(&path, "usage#estimate", 7 * HOUR, 80, true);

        migrate_database(&path).unwrap();

        assert_eq!(
            rows(&path),
            vec![
                ("".into(), 0, 0, false),
                ("five_hour".into(), 2 * HOUR, 90, false),
                ("usage#estimate".into(), 7 * HOUR, 80, true),
                ("weekly#estimate".into(), 5 * HOUR, 100, true),
            ]
        );
        let before = rows(&path);
        migrate_database(&path).unwrap();
        assert_eq!(rows(&path), before, "a second run changes nothing");
    }

    /// The destination already holds an estimate (a database used by both builds): the later
    /// observation stands, whichever row it was written under.
    #[test]
    fn a_conflict_keeps_the_later_observation_in_either_order() {
        // (legacy until/at, destination until/at, expected until/at)
        for (legacy, destination, expected) in [
            ((9 * HOUR, 300), (4 * HOUR, 200), (9 * HOUR, 300)),
            ((9 * HOUR, 100), (4 * HOUR, 200), (4 * HOUR, 200)),
        ] {
            let (_dir, path) = database();
            insert(&path, "weekly", legacy.0, legacy.1, true);
            insert(&path, "weekly#estimate", destination.0, destination.1, true);

            migrate_database(&path).unwrap();

            assert_eq!(
                rows(&path),
                vec![("weekly#estimate".into(), expected.0, expected.1, true)],
                "{legacy:?} / {destination:?}"
            );
        }
    }

    #[test]
    fn a_missing_database_or_table_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        migrate_database(&dir.path().join("acp_events.db")).unwrap();
        let path = dir.path().join("other.db");
        rusqlite::Connection::open(&path).unwrap();
        migrate_database(&path).unwrap();
    }
}
