//! Per-account rate-limit and login state, keyed by the account's resolved config root.
//!
//! Outside the per-session event log so every session sees what any session learned.
//! Rows expire on their own; nothing here blocks an account indefinitely.

use rusqlite::params;

use super::{logged, EventStore};

pub(super) const ACCOUNT_LIMITS_TABLE: &str = "acp_account_limits";

/// Rows whose last limit report is older than this and that hold no deadline in force are
/// dropped on the next write.
const ACCOUNT_STATE_RETENTION_MS: i64 = 7 * 24 * 3600 * 1000;

/// The row kind holding a failed login; limit reports use their own quota-window name.
const LOGIN_ROW: &str = "";

/// Creates the table: one row per account and quota window, plus one login row per account.
pub(super) fn ensure_schema(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {ACCOUNT_LIMITS_TABLE} (
            account_key       TEXT    NOT NULL,
            window_kind       TEXT    NOT NULL,
            limited_until_ms  INTEGER NOT NULL DEFAULT 0,
            unusable_until_ms INTEGER NOT NULL DEFAULT 0,
            updated_at_ms     INTEGER NOT NULL,
            estimated         INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (account_key, window_kind)
        );"
    ))?;
    Ok(())
}

/// One quota window's reported limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowLimit {
    pub kind: String,
    pub until_ms: i64,
    /// `until_ms` is a guess (no reset time was reported), not a reported deadline.
    pub estimated: bool,
}

/// What is known about one account at a point in time.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AccountState {
    /// Each quota window's own deadline; windows expire independently.
    pub windows: Vec<WindowLimit>,
    pub unusable_until_ms: i64,
}

impl AccountState {
    /// The limit recorded for quota window `kind`.
    pub fn window(&self, kind: &str) -> Option<&WindowLimit> {
        self.windows.iter().find(|window| window.kind == kind)
    }

    /// Whether a session may be sent to this account at `now_ms`: every window has elapsed and
    /// the login is usable.
    pub fn has_headroom(&self, now_ms: i64) -> bool {
        self.unusable_until_ms <= now_ms && self.windows.iter().all(|w| w.until_ms <= now_ms)
    }
}

impl EventStore {
    /// Record that `account_key`'s quota window `window_kind` is limited until `limited_until_ms`.
    /// Windows are independent: a report never changes another window's deadline. Within one
    /// window a report observed earlier than the stored one never overwrites it, so replaying the
    /// same park is idempotent, and an `estimated` deadline (the park carried no reset time) never
    /// replaces a reported one that is still in the future.
    pub fn record_account_limit(
        &self,
        account_key: &str,
        limited_until_ms: i64,
        window_kind: &str,
        observed_at_ms: i64,
        estimated: bool,
    ) {
        let window_kind = if window_kind == LOGIN_ROW {
            "unknown"
        } else {
            window_kind
        };
        let conn = self.conn();
        logged(
            conn.execute(
                &format!(
                    "INSERT INTO {ACCOUNT_LIMITS_TABLE}
                        (account_key, window_kind, limited_until_ms, updated_at_ms, estimated)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(account_key, window_kind) DO UPDATE SET
                        limited_until_ms = excluded.limited_until_ms,
                        updated_at_ms = excluded.updated_at_ms,
                        estimated = excluded.estimated
                     WHERE excluded.updated_at_ms >= {ACCOUNT_LIMITS_TABLE}.updated_at_ms
                       AND NOT (excluded.estimated = 1
                                AND {ACCOUNT_LIMITS_TABLE}.estimated = 0
                                AND {ACCOUNT_LIMITS_TABLE}.limited_until_ms > excluded.updated_at_ms)"
                ),
                params![
                    account_key,
                    window_kind,
                    limited_until_ms,
                    observed_at_ms,
                    estimated
                ],
            ),
            "record_account_limit",
            account_key,
        );
        prune(&conn, observed_at_ms);
    }

    /// Record that `account_key` cannot authenticate until `unusable_until_ms`. The later
    /// deadline wins, so replaying one startup error does not extend it. A login failure is not a
    /// limit report: it has its own row and orders nothing.
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
                        (account_key, window_kind, unusable_until_ms, updated_at_ms)
                     VALUES (?1, ?2, ?3, 0)
                     ON CONFLICT(account_key, window_kind) DO UPDATE SET
                        unusable_until_ms = MAX(unusable_until_ms, excluded.unusable_until_ms)"
                ),
                params![account_key, LOGIN_ROW, unusable_until_ms],
            ),
            "record_account_unusable",
            account_key,
        );
        prune(&conn, observed_at_ms);
    }

    /// The stored state of `account_key`, `None` for an account never seen, or the lookup's
    /// error: a failed read says nothing about the account and must not read as headroom.
    pub fn account_state_checked(
        &self,
        account_key: &str,
    ) -> rusqlite::Result<Option<AccountState>> {
        let conn = self.conn();
        let mut statement = conn.prepare(&format!(
            "SELECT window_kind, limited_until_ms, unusable_until_ms, estimated
             FROM {ACCOUNT_LIMITS_TABLE} WHERE account_key = ?1"
        ))?;
        let rows = statement.query_map(params![account_key], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, bool>(3)?,
            ))
        })?;
        let mut state = AccountState::default();
        let mut seen = false;
        for row in rows {
            let (kind, until_ms, unusable_until_ms, estimated) = row?;
            seen = true;
            state.unusable_until_ms = state.unusable_until_ms.max(unusable_until_ms);
            if kind != LOGIN_ROW {
                state.windows.push(WindowLimit {
                    kind,
                    until_ms,
                    estimated,
                });
            }
        }
        Ok(seen.then_some(state))
    }

    /// The stored state of `account_key`, or `None` for an account never seen or a failed read.
    pub fn account_state(&self, account_key: &str) -> Option<AccountState> {
        logged(
            self.account_state_checked(account_key),
            "account_state",
            account_key,
        )
        .flatten()
    }
}

fn prune(conn: &rusqlite::Connection, now_ms: i64) {
    logged(
        conn.execute(
            &format!(
                "DELETE FROM {ACCOUNT_LIMITS_TABLE}
                 WHERE updated_at_ms < ?1 AND limited_until_ms <= ?2 AND unusable_until_ms <= ?2"
            ),
            params![now_ms - ACCOUNT_STATE_RETENTION_MS, now_ms],
        ),
        "prune account limits",
        "",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3_600_000;
    const WEEK: i64 = 7 * 24 * HOUR;

    fn store() -> (tempfile::TempDir, EventStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = EventStore::open(&dir.path().join("events.db"), 100).unwrap();
        (dir, store)
    }

    fn until(store: &EventStore, key: &str, kind: &str) -> Option<i64> {
        store
            .account_state(key)
            .and_then(|state| state.window(kind).map(|window| window.until_ms))
    }

    #[test]
    fn limit_state_is_idempotent_and_newer_reports_win() {
        let (_dir, store) = store();
        assert_eq!(store.account_state("/a"), None);

        store.record_account_limit("/a", 5_000, "usage", 1_000, false);
        store.record_account_limit("/a", 5_000, "usage", 1_000, false);
        let state = store.account_state("/a").unwrap();
        assert_eq!(until(&store, "/a", "usage"), Some(5_000));
        assert!(!state.has_headroom(4_999) && state.has_headroom(5_000));

        // A late report of an older park must not shorten the newer one.
        store.record_account_limit("/a", 2_000, "usage", 500, false);
        assert_eq!(until(&store, "/a", "usage"), Some(5_000));
        // A newer park of the same window replaces it, even with an earlier reset.
        store.record_account_limit("/a", 3_000, "usage", 2_000, false);
        assert_eq!(until(&store, "/a", "usage"), Some(3_000));
    }

    /// The weekly reset was reported with a date. A later park on the same account that
    /// carried none only has an estimate (the 1h doubling schedule), which is its own window's.
    #[test]
    fn an_estimated_deadline_never_shortens_a_dated_one() {
        let (_dir, store) = store();
        store.record_account_limit("/a", WEEK, "weekly", 1_000, false);

        store.record_account_limit("/a", 4 * HOUR, "usage", 2_000, true);
        let state = store.account_state("/a").unwrap();
        assert!(
            !state.has_headroom(5 * HOUR),
            "the weekly window still holds"
        );
        assert_eq!(until(&store, "/a", "weekly"), Some(WEEK));

        // Within one window an estimate never replaces a reported deadline still in force.
        store.record_account_limit("/a", 2 * HOUR, "weekly", 3_000, true);
        assert_eq!(until(&store, "/a", "weekly"), Some(WEEK));
        // Once the dated deadline has passed an estimate is a fresh limit again.
        store.record_account_limit("/a", WEEK + HOUR, "weekly", WEEK + 1_000, true);
        assert_eq!(until(&store, "/a", "weekly"), Some(WEEK + HOUR));
        // A newer dated report wins over an estimate.
        store.record_account_limit("/a", 2 * WEEK, "weekly", WEEK + 2_000, false);
        assert_eq!(until(&store, "/a", "weekly"), Some(2 * WEEK));
        // An estimate stays replaceable by another estimate.
        store.record_account_limit("/b", 100, "usage", 10, true);
        store.record_account_limit("/b", 50, "usage", 20, true);
        assert_eq!(until(&store, "/b", "usage"), Some(50));
    }

    /// Weekly, then usage 4h, usage 5h, and a newer usage 2h: the account is held until the
    /// weekly reset, whatever the usage window later says.
    #[test]
    fn windows_keep_their_own_deadline() {
        let (_dir, store) = store();
        store.record_account_limit("/a", WEEK, "weekly", 1_000, false);
        store.record_account_limit("/a", 4 * HOUR, "usage", 2_000, false);
        store.record_account_limit("/a", 5 * HOUR, "usage", 3_000, false);
        store.record_account_limit("/a", 2 * HOUR, "usage", 4_000, false);
        let state = store.account_state("/a").unwrap();
        assert_eq!(until(&store, "/a", "usage"), Some(2 * HOUR));
        assert!(!state.has_headroom(3 * HOUR), "usage elapsed, weekly not");
        assert!(!state.has_headroom(WEEK - 1));
        assert!(state.has_headroom(WEEK));
    }

    /// A usage park that replaced the usage row must not make a delayed, longer weekly
    /// report lose to a global timestamp.
    #[test]
    fn a_delayed_report_for_another_window_still_lands() {
        let (_dir, store) = store();
        store.record_account_limit("/a", 5 * HOUR, "usage", 3_000, false);
        store.record_account_limit("/a", 2 * HOUR, "usage", 4_000, false);
        store.record_account_limit("/a", WEEK, "weekly", 1_000, false);
        let state = store.account_state("/a").unwrap();
        assert_eq!(until(&store, "/a", "weekly"), Some(WEEK));
        assert!(!state.has_headroom(3 * HOUR));
        assert!(state.has_headroom(WEEK));
        assert_eq!(
            until(&store, "/a", "monthly"),
            None,
            "an unreported window has no deadline"
        );
    }

    /// A login failure does not order limit reports: a limit observed before it but reported
    /// after still lands.
    #[test]
    fn a_delayed_limit_report_survives_a_later_login_failure() {
        let (_dir, store) = store();
        store.record_account_unusable("/a", 2_000 + HOUR, 2_000);
        store.record_account_limit("/a", 10 * HOUR, "usage", 1_000, false);
        let state = store.account_state("/a").unwrap();
        assert_eq!(until(&store, "/a", "usage"), Some(10 * HOUR));
        assert!(!state.has_headroom(5 * HOUR));
        store.record_account_limit("/b", 10 * HOUR, "usage", 1_000, false);
        store.record_account_unusable("/b", 2_000 + HOUR, 2_000);
        store.record_account_limit("/b", 9 * HOUR, "usage", 900, false);
        assert_eq!(
            until(&store, "/b", "usage"),
            Some(10 * HOUR),
            "an older limit report is still dropped"
        );
    }

    #[test]
    fn unusable_state_expires_and_keeps_the_later_deadline() {
        let (_dir, store) = store();
        store.record_account_unusable("/b", 9_000, 1_000);
        store.record_account_unusable("/b", 4_000, 1_500);
        let state = store.account_state("/b").unwrap();
        assert_eq!(state.unusable_until_ms, 9_000);
        assert!(state.windows.is_empty());
        assert!(!state.has_headroom(8_999));
        assert!(state.has_headroom(9_000), "the entry expires by itself");
    }

    /// An account untouched past retention that fails auth now keeps its fresh mark, and a
    /// deadline still in force survives pruning whatever its age.
    #[test]
    fn a_fresh_unusable_mark_survives_pruning_of_an_old_row() {
        let (_dir, store) = store();
        let now = ACCOUNT_STATE_RETENTION_MS * 3;
        store.record_account_limit("/a", 10, "usage", 10, false);
        store.record_account_unusable("/a", now + 60_000, now);
        assert!(!store.account_state("/a").unwrap().has_headroom(now + 1));

        store.record_account_limit(
            "/w",
            now + 5 * ACCOUNT_STATE_RETENTION_MS,
            "weekly",
            10,
            false,
        );
        store.record_account_limit("/other", 10, "usage", now, false);
        assert!(store.account_state("/w").is_some());
    }

    #[test]
    fn stale_rows_are_pruned_on_write() {
        let (_dir, store) = store();
        store.record_account_limit("/old", 10, "usage", 10, false);
        store.record_account_limit("/new", 10, "usage", ACCOUNT_STATE_RETENTION_MS + 20, false);
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
        reopened.record_account_limit("/a", 10, "usage", 5, false);
        assert!(reopened.account_state("/a").is_some());
        drop(reopened);
        let again = EventStore::open(&path, 100).unwrap();
        assert!(again.account_state("/a").is_some());
    }
}
