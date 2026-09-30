//! SQLite persistence for usage events.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};

use super::{format_occurred_at, summarize, UsageEvent, UsageKind, UsageSummary};

const SCHEMA_VERSION: &str = "1";

pub struct UsageStore {
    conn: Connection,
}

impl UsageStore {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("open usage log at {}", path.display()))?;
        conn.busy_timeout(Duration::from_millis(2000))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS usage_events (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 occurred_at TEXT NOT NULL,
                 instance_id TEXT NOT NULL,
                 profile TEXT,
                 agent TEXT,
                 kind TEXT NOT NULL,
                 detail TEXT,
                 agent_session_id TEXT
             );
             CREATE INDEX IF NOT EXISTS usage_events_instance
                 ON usage_events(instance_id, id);
             CREATE TABLE IF NOT EXISTS usage_meta (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             INSERT OR IGNORE INTO usage_meta(key, value) VALUES ('schema_version', '1');",
        )?;
        let version: Option<String> = conn
            .query_row(
                "SELECT value FROM usage_meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if version.as_deref() != Some(SCHEMA_VERSION) {
            bail!("unsupported usage log schema version {version:?}");
        }
        Ok(Self { conn })
    }

    pub fn open_default() -> Result<Self> {
        Self::open(&super::db_path()?)
    }

    pub fn insert(&self, event: &UsageEvent) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO usage_events
                 (occurred_at, instance_id, profile, agent, kind, detail, agent_session_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                format_occurred_at(&event.occurred_at),
                event.instance_id,
                event.profile,
                event.agent,
                event.kind.as_str(),
                event.detail,
                event.agent_session_id,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn events_for_instance(&self, instance_id: &str) -> Result<Vec<UsageEvent>> {
        self.query("WHERE instance_id = ?1 ORDER BY id", params![instance_id])
    }

    pub fn summary_for(&self, instance_id: &str) -> Result<UsageSummary> {
        Ok(summarize(&self.events_for_instance(instance_id)?))
    }

    /// The export cursor: rows with `id > after_id`, oldest first.
    pub fn events_after(&self, after_id: i64, limit: usize) -> Result<Vec<UsageEvent>> {
        self.query(
            "WHERE id > ?1 ORDER BY id LIMIT ?2",
            params![after_id, limit as i64],
        )
    }

    pub fn events_since(&self, since: DateTime<Utc>) -> Result<Vec<UsageEvent>> {
        self.query(
            "WHERE occurred_at >= ?1 ORDER BY id",
            params![format_occurred_at(&since)],
        )
    }

    fn query(&self, clause: &str, params: impl rusqlite::Params) -> Result<Vec<UsageEvent>> {
        let sql = format!(
            "SELECT id, occurred_at, instance_id, profile, agent, kind, detail, agent_session_id
             FROM usage_events {clause}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params, read_row)?;
        let mut events = Vec::new();
        for row in rows {
            // A row this build cannot read (an unknown kind) is skipped, not fatal.
            if let Some(event) = row? {
                events.push(event);
            }
        }
        Ok(events)
    }
}

fn read_row(row: &Row<'_>) -> rusqlite::Result<Option<UsageEvent>> {
    let occurred_at: String = row.get(1)?;
    let kind: String = row.get(5)?;
    let (Ok(occurred_at), Some(kind)) = (
        DateTime::parse_from_rfc3339(&occurred_at),
        UsageKind::parse(&kind),
    ) else {
        return Ok(None);
    };
    Ok(Some(UsageEvent {
        id: row.get(0)?,
        occurred_at: occurred_at.with_timezone(&Utc),
        instance_id: row.get(2)?,
        profile: row.get(3)?,
        agent: row.get(4)?,
        kind,
        detail: row.get(6)?,
        agent_session_id: row.get(7)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::summary::tests::ev;

    #[test]
    fn events_round_trip_and_filter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.db");
        let store = UsageStore::open(&path).unwrap();
        let mut first = ev(0, UsageKind::ContextStart, Some("startup"));
        first.agent_session_id = Some("sid-1".into());
        first.profile = Some("work".into());
        let mut other = ev(1, UsageKind::Prompt, None);
        other.instance_id = "other".into();
        let third = ev(2, UsageKind::ContextStart, Some("clear"));
        let ids: Vec<i64> = [&first, &other, &third]
            .iter()
            .map(|e| store.insert(e).unwrap())
            .collect();
        assert!(ids.windows(2).all(|w| w[0] < w[1]));

        let mine = store.events_for_instance("inst").unwrap();
        assert_eq!(mine.len(), 2);
        assert_eq!(mine[0].id, ids[0]);
        assert_eq!(
            UsageEvent {
                id: 0,
                ..mine[0].clone()
            },
            first,
            "every column survives the round trip"
        );
        assert_eq!(store.summary_for("inst").unwrap().clears, 1);
        assert_eq!(store.events_after(ids[0], 10).unwrap().len(), 2);
        assert_eq!(store.events_after(ids[0], 1).unwrap()[0].id, ids[1]);
        assert_eq!(store.events_since(third.occurred_at).unwrap().len(), 1);

        drop(store);
        let reopened = UsageStore::open(&path).unwrap();
        assert_eq!(reopened.events_after(0, 100).unwrap().len(), 3);
    }

    #[test]
    fn a_newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.db");
        drop(UsageStore::open(&path).unwrap());
        Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE usage_meta SET value = '2' WHERE key = 'schema_version'",
                [],
            )
            .unwrap();
        assert!(UsageStore::open(&path).is_err());
    }
}
