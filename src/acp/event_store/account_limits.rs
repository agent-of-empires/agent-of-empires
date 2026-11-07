//! Per-account rate-limit and login state, keyed by the account's resolved config root.
//!
//! Outside the per-session event log so every session sees what any session learned.
//! Rows expire on their own; nothing here blocks an account indefinitely.

use rusqlite::{params, OptionalExtension};

use super::{logged, EventStore};

pub(super) const ACCOUNT_LIMITS_TABLE: &str = "acp_account_limits";

/// Rows untouched for this long are dropped on the next write.
const ACCOUNT_STATE_RETENTION_MS: i64 = 7 * 24 * 3600 * 1000;

pub(super) fn create_table_sql() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {ACCOUNT_LIMITS_TABLE} (
            account_key      TEXT    NOT NULL PRIMARY KEY,
            limited_until_ms INTEGER NOT NULL DEFAULT 0,
            window_kind      TEXT    NOT NULL DEFAULT '',
            unusable_until_ms INTEGER NOT NULL DEFAULT 0,
            updated_at_ms    INTEGER NOT NULL
        );"
    )
}

/// What is known about one account at a point in time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountState {
    pub limited_until_ms: i64,
    pub window_kind: String,
    pub unusable_until_ms: i64,
}

impl AccountState {
    /// Whether a session may be sent to this account at `now_ms`.
    pub fn has_headroom(&self, now_ms: i64) -> bool {
        self.limited_until_ms <= now_ms && self.unusable_until_ms <= now_ms
    }
}

impl EventStore {
    /// Record that `account_key` is limited until `limited_until_ms`. A report
    /// observed earlier than the stored one never overwrites it, so replaying
    /// the same park is idempotent.
    pub fn record_account_limit(
        &self,
        account_key: &str,
        limited_until_ms: i64,
        window_kind: &str,
        observed_at_ms: i64,
    ) {
        let conn = self.conn();
        logged(
            conn.execute(
                &format!(
                    "INSERT INTO {ACCOUNT_LIMITS_TABLE}
                        (account_key, limited_until_ms, window_kind, updated_at_ms)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(account_key) DO UPDATE SET
                        limited_until_ms = excluded.limited_until_ms,
                        window_kind = excluded.window_kind,
                        updated_at_ms = excluded.updated_at_ms
                     WHERE excluded.updated_at_ms >= {ACCOUNT_LIMITS_TABLE}.updated_at_ms"
                ),
                params![account_key, limited_until_ms, window_kind, observed_at_ms],
            ),
            "record_account_limit",
            account_key,
        );
        prune(&conn, observed_at_ms);
    }

    /// Record that `account_key` cannot authenticate until `unusable_until_ms`.
    /// The later deadline wins, so replaying one startup error does not extend it.
    pub fn record_account_unusable(
        &self,
        account_key: &str,
        unusable_until_ms: i64,
        observed_at_ms: i64,
    ) {
        let conn = self.conn();
        logged(
            conn.execute(
                &format!(
                    "INSERT INTO {ACCOUNT_LIMITS_TABLE}
                        (account_key, unusable_until_ms, updated_at_ms)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(account_key) DO UPDATE SET
                        unusable_until_ms = MAX(unusable_until_ms, excluded.unusable_until_ms)"
                ),
                params![account_key, unusable_until_ms, observed_at_ms],
            ),
            "record_account_unusable",
            account_key,
        );
        prune(&conn, observed_at_ms);
    }

    /// The stored state of `account_key`, or `None` for an account never seen.
    pub fn account_state(&self, account_key: &str) -> Option<AccountState> {
        logged(
            self.conn()
                .query_row(
                    &format!(
                        "SELECT limited_until_ms, window_kind, unusable_until_ms
                         FROM {ACCOUNT_LIMITS_TABLE} WHERE account_key = ?1"
                    ),
                    params![account_key],
                    |row| {
                        Ok(AccountState {
                            limited_until_ms: row.get(0)?,
                            window_kind: row.get(1)?,
                            unusable_until_ms: row.get(2)?,
                        })
                    },
                )
                .optional(),
            "account_state",
            account_key,
        )
        .flatten()
    }
}

fn prune(conn: &rusqlite::Connection, now_ms: i64) {
    logged(
        conn.execute(
            &format!("DELETE FROM {ACCOUNT_LIMITS_TABLE} WHERE updated_at_ms < ?1"),
            params![now_ms - ACCOUNT_STATE_RETENTION_MS],
        ),
        "prune account limits",
        "",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, EventStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = EventStore::open(&dir.path().join("events.db"), 100).unwrap();
        (dir, store)
    }

    #[test]
    fn limit_state_is_idempotent_and_newer_reports_win() {
        let (_dir, store) = store();
        assert_eq!(store.account_state("/a"), None);

        store.record_account_limit("/a", 5_000, "usage", 1_000);
        store.record_account_limit("/a", 5_000, "usage", 1_000);
        let state = store.account_state("/a").unwrap();
        assert_eq!(
            (state.limited_until_ms, state.window_kind.as_str()),
            (5_000, "usage")
        );
        assert!(!state.has_headroom(4_999) && state.has_headroom(5_000));

        // A late report of an older park must not shorten the newer one.
        store.record_account_limit("/a", 2_000, "usage", 500);
        assert_eq!(store.account_state("/a").unwrap().limited_until_ms, 5_000);
        // A newer park replaces it, even with an earlier reset.
        store.record_account_limit("/a", 3_000, "weekly", 2_000);
        let state = store.account_state("/a").unwrap();
        assert_eq!(
            (state.limited_until_ms, state.window_kind.as_str()),
            (3_000, "weekly")
        );
    }

    #[test]
    fn unusable_state_expires_and_keeps_the_later_deadline() {
        let (_dir, store) = store();
        store.record_account_unusable("/b", 9_000, 1_000);
        store.record_account_unusable("/b", 4_000, 1_500);
        let state = store.account_state("/b").unwrap();
        assert_eq!(state.unusable_until_ms, 9_000);
        assert!(!state.has_headroom(8_999));
        assert!(state.has_headroom(9_000), "the entry expires by itself");
    }

    #[test]
    fn stale_rows_are_pruned_on_write() {
        let (_dir, store) = store();
        store.record_account_limit("/old", 10, "usage", 10);
        store.record_account_limit("/new", 10, "usage", ACCOUNT_STATE_RETENTION_MS + 20);
        assert_eq!(store.account_state("/old"), None);
        assert!(store.account_state("/new").is_some());
    }

    /// A database written before this table existed gains it on open and
    /// keeps its events.
    #[test]
    fn a_database_without_the_table_is_upgraded_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        let store = EventStore::open(&path, 100).unwrap();
        store
            .record(
                "s-1",
                1,
                &crate::acp::state::Event::Stopped {
                    reason: "end_turn".into(),
                },
            )
            .unwrap();
        store
            .conn()
            .execute_batch(&format!("DROP TABLE {ACCOUNT_LIMITS_TABLE};"))
            .unwrap();
        drop(store);

        let reopened = EventStore::open(&path, 100).unwrap();
        assert_eq!(reopened.highest_seq("s-1"), 1, "existing events survive");
        reopened.record_account_limit("/a", 10, "usage", 5);
        assert!(reopened.account_state("/a").is_some());
        // Opening twice over the upgraded file is a no-op.
        drop(reopened);
        let again = EventStore::open(&path, 100).unwrap();
        assert!(again.account_state("/a").is_some());
    }
}
