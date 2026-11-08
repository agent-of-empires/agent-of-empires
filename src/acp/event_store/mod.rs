//! Disk-backed event log for structured view sessions.

// Management admission does not end work retained by the native runner. Keep
// this predicate identical across turn, responder, monitor and budget queries.
macro_rules! native_channel_loss_sql {
    ($json:literal) => {
        concat!(
            "(json_extract(",
            $json,
            ", '$.AcpConnectionUnavailable.source') IN ('sdk','control','native')",
            " AND json_type(",
            $json,
            ", '$.AcpConnectionUnavailable.native_subject.kind.AdmissionRefusal') IS NULL)"
        )
    };
}

mod attachments;
pub(crate) mod control;
mod establishment;
mod rate_limit;
mod replay;
mod search;
mod turns;
mod wakeups;

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use tracing::{debug, trace, warn};

use super::event_sender::EventMetadata;
#[cfg(test)]
use super::event_sender::EventScope;
use super::state::Event;
use crate::events;

pub use attachments::AttachmentBlob;
pub use control::ControlKnowledge;
pub(crate) use establishment::EstablishmentSeal;
pub use establishment::ReplayCommit;
pub use rate_limit::RateLimitPark;
pub use replay::ReplayPage;
pub use search::ContentHit;
pub use turns::{TerminalRepairProbe, UnresolvedBackgroundAgentLaunch};

/// Lifecycle and metadata events that do not count as session activity. All
/// of them are also retention-pinned.
const NON_SUBSTANTIVE_EVENT_DISCRIMINANTS: &[&str] = &[
    "AvailableCommandsUpdated",
    "ModesAvailable",
    "CurrentModeChanged",
    "AcpSessionAssigned",
    "PromptCapabilities",
    "AuthStatusUpdated",
];

/// Background work as its (start, end, id field) events. Teardown and cold
/// folds find open work by pairing them, so the retention prune pins both
/// halves, and [`prune_completed_background_work`] drops a pair once it ends.
const BACKGROUND_WORK_EVENTS: &[(&str, &str, &str)] = &[
    ("BackgroundTaskStarted", "BackgroundTaskFinished", "task_id"),
    (
        "BackgroundAgentLaunched",
        "BackgroundAgentCompleted",
        "agent_id",
    ),
];

/// How often (in seqs) a session's completed background work is swept, which
/// amortizes the delete scans.
const COMPLETED_WORK_SWEEP_INTERVAL: u64 = 64;

/// Rows the retention prune keeps: the non-substantive events above, plus the
/// background work events of [`BACKGROUND_WORK_EVENTS`].
const RETENTION_PINNED_EVENT_DISCRIMINANTS: &[&str] = &[
    "AvailableCommandsUpdated",
    "ModesAvailable",
    "CurrentModeChanged",
    "AcpSessionAssigned",
    "AcpConnectionUnavailable",
    "SessionContextReset",
    "SessionCleared",
    "PromptCapabilities",
    "AuthStatusUpdated",
    "BackgroundAgentLaunched",
    "BackgroundAgentCompleted",
    "BackgroundTaskStarted",
    "BackgroundTaskFinished",
];

/// SQLite-backed structured view event log.
pub struct EventStore {
    conn: Mutex<Connection>,
    /// Read-only connection used exclusively by `search_content`.
    search_conn: Mutex<Connection>,
    schema: events::Schema,
    max_events_per_session: usize,
    control_cache: Arc<super::control_cache::ControlStateCache>,
}

#[derive(Debug, Clone)]
pub struct StoredEvent {
    pub seq: u64,
    pub event: std::sync::Arc<Event>,
    pub(crate) metadata: EventMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordDisposition {
    Inserted,
    Existing { seq: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventTableShape {
    Absent,
    Legacy { discriminant: bool },
    Scoped,
}

impl EventStore {
    pub(crate) fn event_table_shape(conn: &Connection) -> Result<EventTableShape> {
        let kind: Option<String> = conn
            .query_row(
                "SELECT type FROM sqlite_schema WHERE name = 'acp_events'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let Some(kind) = kind else {
            return Ok(EventTableShape::Absent);
        };
        anyhow::ensure!(kind == "table", "acp_events is not a table");
        let mut statement = conn.prepare("PRAGMA table_info(acp_events)")?;
        let columns = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, i64>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (name, kind, key) in [
            ("session_id", "TEXT", 1),
            ("seq", "INTEGER", 2),
            ("event_json", "TEXT", 0),
            ("created_at", "INTEGER", 0),
        ] {
            anyhow::ensure!(
                columns.iter().any(|column| column.0 == name
                    && column.1.eq_ignore_ascii_case(kind)
                    && column.2
                    && column.3 == key),
                "incompatible acp_events column {name}"
            );
        }
        let discriminant = columns.iter().find(|column| column.0 == "discriminant");
        anyhow::ensure!(
            discriminant.is_none_or(|column| column.1.eq_ignore_ascii_case("TEXT")),
            "incompatible ACP discriminant"
        );
        let scope = columns.iter().find(|column| column.0 == "scope");
        let qualification = columns
            .iter()
            .find(|column| column.0 == "qualification_json");
        match (scope, qualification) {
            (None, None) => Ok(EventTableShape::Legacy {
                discriminant: discriminant.is_some(),
            }),
            (Some(scope), Some(qualification)) => {
                anyhow::ensure!(
                    discriminant.is_some()
                        && scope.1.eq_ignore_ascii_case("INTEGER")
                        && scope.2
                        && qualification.1.eq_ignore_ascii_case("TEXT"),
                    "incompatible scoped ACP schema"
                );
                Ok(EventTableShape::Scoped)
            }
            _ => anyhow::bail!("incomplete ACP scope migration"),
        }
    }

    pub(crate) fn create_scoped_event_table(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE acp_events (
                session_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                event_json TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                discriminant TEXT,
                scope INTEGER NOT NULL DEFAULT 0 CHECK(scope IN (0,1,2,3)),
                qualification_json TEXT,
                CHECK(scope IN (0,1) OR qualification_json IS NOT NULL),
                PRIMARY KEY(session_id, seq)
            );",
        )?;
        Ok(())
    }

    pub(crate) fn validate_scope_rows(conn: &Connection) -> Result<()> {
        let mut statement = conn.prepare(
            "SELECT session_id, seq, event_json, created_at, discriminant, scope, qualification_json
             FROM acp_events"
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let _: String = row.get(0)?;
            let seq: i64 = row.get(1)?;
            anyhow::ensure!(seq >= 0, "negative ACP sequence");
            let json = row.get_ref(2)?.as_str()?;
            let _: i64 = row.get(3)?;
            let _: Option<String> = row.get(4)?;
            serde_json::from_str::<serde::de::IgnoredAny>(json)
                .with_context(|| format!("invalid ACP JSON at sequence {seq}"))?;
            let metadata = EventMetadata::decode_private(
                row.get(5)?,
                row.get::<_, Option<String>>(6)?.as_deref(),
            )
            .with_context(|| format!("invalid ACP qualification at sequence {seq}"))?;
            if metadata.availability_denial {
                metadata.validate_for_event(&serde_json::from_str::<Event>(json)?)?;
            }
        }
        Ok(())
    }

    pub(crate) fn validate_auxiliary_tables(conn: &Connection) -> Result<()> {
        for (table, expected) in [
            (
                "acp_attachments",
                &[
                    ("session_id", "TEXT", true, 1),
                    ("seq", "INTEGER", true, 0),
                    ("attachment_id", "TEXT", true, 2),
                    ("kind", "TEXT", true, 0),
                    ("mime_type", "TEXT", true, 0),
                    ("name", "TEXT", false, 0),
                    ("data", "BLOB", true, 0),
                    ("created_at", "INTEGER", true, 0),
                ][..],
            ),
            (
                "acp_pending_attachments",
                &[
                    ("session_id", "TEXT", true, 1),
                    ("ref_id", "TEXT", true, 0),
                    ("attachment_id", "TEXT", true, 2),
                    ("kind", "TEXT", true, 0),
                    ("mime_type", "TEXT", true, 0),
                    ("name", "TEXT", false, 0),
                    ("data", "BLOB", true, 0),
                    ("created_at", "INTEGER", true, 0),
                ][..],
            ),
            (
                "acp_rate_limit_budgets",
                &[
                    ("session_id", "TEXT", true, 1),
                    ("spent", "INTEGER", true, 0),
                    ("armed", "INTEGER", true, 0),
                ][..],
            ),
            (
                "acp_establishment_commit",
                &[
                    ("session_id", "TEXT", true, 1),
                    ("seal_json", "TEXT", true, 0),
                ][..],
            ),
        ] {
            let mut statement =
                conn.prepare("SELECT name, type, \"notnull\", pk FROM pragma_table_info(?1)")?;
            let mut rows = statement.query([table])?;
            let mut seen = 0u16;
            while let Some(row) = rows.next()? {
                let name = row.get_ref(0)?.as_str()?;
                if let Some((index, (_, kind, not_null, key))) = expected
                    .iter()
                    .enumerate()
                    .find(|(_, column)| column.0 == name)
                {
                    anyhow::ensure!(
                        row.get_ref(1)?.as_str()?.eq_ignore_ascii_case(kind)
                            && row.get::<_, bool>(2)? == *not_null
                            && row.get::<_, i64>(3)? == *key,
                        "incompatible {table} column {name}"
                    );
                    seen |= 1 << index;
                }
            }
            anyhow::ensure!(
                seen == (1 << expected.len()) - 1,
                "incomplete {table} schema"
            );
        }
        Ok(())
    }

    pub(crate) fn preserve_legacy_budgets(
        tx: &rusqlite::Transaction<'_>,
        schema: &events::Schema,
    ) -> Result<()> {
        rate_limit::preserve_legacy_budgets(tx, schema)
    }

    /// Open or create the database at `db_path`.
    pub fn open(db_path: &Path, max_events_per_session: usize) -> Result<Self> {
        // Prefix "acp" maps to the existing acp_events / acp_attachments tables.
        let schema = events::Schema::new("acp")?;
        let mut conn = events::connect(db_path)?;
        match Self::event_table_shape(&conn)? {
            EventTableShape::Absent => {
                let tx = conn.transaction()?;
                Self::create_scoped_event_table(&tx)?;
                events::initialize_schema(&tx, &schema)?;
                tx.execute_batch(
                    "ALTER TABLE acp_rate_limit_budgets
                     ADD COLUMN known INTEGER NOT NULL DEFAULT 1 CHECK (known IN (0,1));",
                )?;
                Self::create_establishment_metadata(&tx)?;
                tx.commit()?;
            }
            EventTableShape::Scoped => {
                Self::validate_auxiliary_tables(&conn)?;
                Self::validate_prompt_commit_schema(&conn)?;
                Self::validate_budget_knowledge(&conn)?;
                events::initialize_schema(&conn, &schema)?;
            }
            EventTableShape::Legacy { .. } => {
                anyhow::bail!("ACP event database requires migration55 before opening")
            }
        }
        let search_conn = Connection::open_with_flags(
            db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .with_context(|| format!("open read-only search handle at {}", db_path.display()))?;
        search_conn
            .pragma_update(None, "query_only", "ON")
            .context("set query_only=ON on search handle")?;
        search_conn
            .busy_timeout(std::time::Duration::from_millis(1000))
            .context("set busy_timeout on search handle")?;
        debug!(
            target: "acp.event_store",
            path = %db_path.display(),
            cap = max_events_per_session,
            "structured view event store opened"
        );
        Ok(Self {
            conn: Mutex::new(conn),
            search_conn: Mutex::new(search_conn),
            schema,
            max_events_per_session,
            control_cache: Arc::new(super::control_cache::ControlStateCache::new()),
        })
    }

    pub fn control_cache(&self) -> &Arc<super::control_cache::ControlStateCache> {
        &self.control_cache
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[cfg(test)]
    pub(crate) fn test_conn(&self) -> MutexGuard<'_, Connection> {
        self.conn()
    }

    /// Public serde omits the subject; only the private journal adds it back.
    fn encode_event(event: &Event) -> Result<String> {
        let Some(subject) = event.native_subject() else {
            return Ok(serde_json::to_string(event)?);
        };
        let mut json = serde_json::to_vec(event)?;
        // These are externally tagged struct variants: {"Variant":{...}}.
        anyhow::ensure!(
            json.ends_with(b"}}"),
            "native subject requires a structured event payload"
        );
        json.truncate(json.len() - 2);
        json.extend_from_slice(b",\"native_subject\":");
        serde_json::to_writer(&mut json, &subject)?;
        json.extend_from_slice(b"}}");
        String::from_utf8(json).context("event serializer produced invalid UTF-8")
    }

    fn insert_scoped_event(
        tx: &rusqlite::Transaction<'_>,
        session_id: &str,
        seq: u64,
        json: &str,
        created_at: i64,
        metadata: &EventMetadata,
    ) -> Result<usize> {
        let qualification = metadata.encode_private()?;
        let seq = i64::try_from(seq).context("ACP sequence exceeds SQLite range")?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO acp_events
                (session_id, seq, event_json, created_at, discriminant, scope, qualification_json)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            rusqlite::params![
                session_id,
                seq,
                json,
                created_at,
                event_kind(json),
                metadata.scope as i64,
                qualification
            ],
        )?;
        if inserted == 0 {
            let identical: Option<bool> = tx
                .query_row(
                    "SELECT event_json = ?3 AND scope = ?4 AND qualification_json IS ?5
                 FROM acp_events WHERE session_id = ?1 AND seq = ?2",
                    rusqlite::params![session_id, seq, json, metadata.scope as i64, qualification],
                    |row| row.get(0),
                )
                .optional()?;
            anyhow::ensure!(
                identical == Some(true),
                "conflicting ACP journal append at {session_id}@{seq}"
            );
        }
        Ok(inserted)
    }

    /// Append or accept an identical row; reject a conflicting sequence.
    pub(crate) fn record(
        &self,
        session_id: &str,
        seq: u64,
        event: &Event,
        metadata: &EventMetadata,
    ) -> Result<RecordDisposition> {
        metadata.validate_for_event(event)?;
        anyhow::ensure!(
            metadata.clear_after_seq.is_none_or(|cut| cut < seq),
            "conversation display cut must precede its clear"
        );
        let json = Self::encode_event(event)
            .with_context(|| format!("serialise event for {session_id}@{seq}"))?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        let recorded = self.append_controlled(session_id, seq, event, metadata, now_ms, |tx| {
            match Self::record_prompt_result(tx, session_id, seq, event, &json, now_ms, metadata)? {
                Some(recorded) => Ok(recorded),
                None => Ok(
                    match Self::insert_scoped_event(tx, session_id, seq, &json, now_ms, metadata)? {
                        0 => RecordDisposition::Existing { seq },
                        _ => RecordDisposition::Inserted,
                    },
                ),
            }
        })?;
        trace!(
            target: "acp.event_store",
            session = %session_id,
            seq,
            kind = event_kind(&json),
            bytes = json.len(),
            "recorded event"
        );
        Ok(recorded)
    }

    /// Record with an explicit `created_at` (ms epoch) for recency-sensitive tests.
    #[cfg(test)]
    pub(crate) fn record_at(
        &self,
        session_id: &str,
        seq: u64,
        event: &Event,
        created_at_ms: i64,
    ) -> Result<()> {
        let json = Self::encode_event(event)?;
        self.control_cache
            .with_control(session_id, |cursor| -> Result<()> {
                events::insert_event(
                    &self.conn(),
                    &self.schema,
                    session_id,
                    seq,
                    &json,
                    created_at_ms,
                )?;
                *cursor = None;
                Ok(())
            })?;
        self.control_cache.forget(session_id);
        Ok(())
    }

    /// Highest seq stored for `session_id`, or 0.
    pub fn highest_seq(&self, session_id: &str) -> u64 {
        let max = events::highest_seq(&self.conn(), &self.schema, session_id);
        trace!(
            target: "acp.event_store",
            session = %session_id,
            highest_seq = max,
            "highest_seq query"
        );
        max
    }

    /// Lowest seq still stored for `session_id`, or `None` when nothing is.
    pub fn lowest_seq(&self, session_id: &str) -> Option<u64> {
        let min = events::lowest_seq(&self.conn(), &self.schema, session_id);
        trace!(
            target: "acp.event_store",
            session = %session_id,
            lowest_seq = ?min,
            "lowest_seq query"
        );
        min
    }

    /// Publication counters retain sequence IDs after ordinary result rows are pruned.
    pub fn all_session_seqs(&self) -> Result<Vec<(String, u64)>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT session_id, MAX(seq) FROM (
                SELECT session_id, MAX(seq) AS seq FROM acp_events GROUP BY session_id
                UNION ALL
                SELECT session_id, MAX(
                    COALESCE(json_extract(prompt_commit_json, '$.diagnostic.seq'), 0),
                    COALESCE(json_extract(prompt_commit_json, '$.stopped'), 0))
                FROM acp_establishment_commit WHERE prompt_commit_json IS NOT NULL
             ) GROUP BY session_id",
        )?;
        let mut rows = statement.query([])?;
        let mut collected = Vec::new();
        while let Some(row) = rows.next()? {
            let seq = u64::try_from(row.get::<_, i64>(1)?)
                .context("negative ACP publication sequence")?;
            collected.push((row.get::<_, String>(0)?, seq));
        }
        debug!(
            target: "acp.event_store",
            sessions = collected.len(),
            "all_session_seqs hydration"
        );
        Ok(collected)
    }

    pub(crate) fn highest_seq_checked(&self, session_id: &str) -> Result<u64> {
        let seq: i64 = self.conn().query_row(
            &format!(
                "SELECT COALESCE(MAX(seq), 0) FROM {} WHERE session_id = ?1",
                self.schema.events_table()
            ),
            rusqlite::params![session_id],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(seq)?)
    }

    pub(crate) fn handoff_context_event(
        &self,
        session_id: &str,
        after: u64,
        through: u64,
    ) -> Result<Option<(u64, Event)>> {
        let found: Option<(i64, String)> = self.conn().query_row(
        &format!("SELECT seq, event_json FROM {} WHERE session_id = ?1 AND seq > ?2 AND seq <= ?3 AND discriminant IN ('AcpSessionAssigned', 'SessionContextReset', 'SessionCleared') ORDER BY seq DESC LIMIT 1", self.schema.events_table()),
        rusqlite::params![session_id, i64::try_from(after)?, i64::try_from(through)?],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()?;
        found
            .map(|(seq, json)| -> Result<_> {
                Ok((u64::try_from(seq)?, serde_json::from_str(&json)?))
            })
            .transpose()
    }
    pub(crate) fn delete_session_checked(&self, session_id: &str) -> Result<()> {
        self.control_cache
            .with_control(session_id, |cursor| -> Result<()> {
                let mut conn = self.conn();
                let tx = conn.transaction()?;
                for table in [
                    self.schema.events_table(),
                    self.schema.attachments_table(),
                    self.schema.pending_attachments_table(),
                    self.schema.rate_limit_budgets_table(),
                    establishment::TABLE,
                ] {
                    tx.execute(
                        &format!("DELETE FROM {table} WHERE session_id = ?1"),
                        rusqlite::params![session_id],
                    )?;
                }
                tx.commit()?;
                *cursor = None;
                Ok(())
            })?;
        self.control_cache.forget(session_id);
        Ok(())
    }

    pub(crate) fn handoff_stopped_after(&self, session_id: &str, after: u64) -> Result<bool> {
        let conn = self.conn();
        let mut statement = conn.prepare(&format!(
        "SELECT event_json FROM {} WHERE session_id = ?1 AND seq > ?2 AND discriminant = 'Stopped' ORDER BY seq DESC",
        self.schema.events_table(),
    ))?;
        let rows = statement.query_map(
            rusqlite::params![session_id, i64::try_from(after)?],
            |row| row.get::<_, String>(0),
        )?;
        for json in rows {
            match serde_json::from_str::<Event>(&json?)? {
                Event::Stopped { reason } if reason == "user_stopped" => return Ok(true),
                Event::Stopped { .. } => {}
                _ => anyhow::bail!("persisted Stopped discriminant has a different event payload"),
            }
        }
        Ok(false)
    }
    /// Drop every event for a session, cascading to its attachment blobs.
    pub fn delete_session(&self, session_id: &str) {
        if let Err(error) = self.delete_session_checked(session_id) {
            warn!(target: "acp.event_store", session = %session_id,
                "could not delete session events and private commit metadata: {error}");
        }
    }
}

/// Deletes background work that has ended and fell past the retention cutoff,
/// leaving open work pinned. Starts go first, so an end is never removed while
/// its start survives (that start would then read as open).
fn prune_completed_background_work(
    conn: &Connection,
    schema: &events::Schema,
    session_id: &str,
    max_events: usize,
) {
    let events = schema.events_table();
    // The same cutoff `events::prune_retention` uses.
    let cutoff: Option<i64> = conn
        .query_row(
            &format!(
                "SELECT seq FROM {events} WHERE session_id = ?1
                 ORDER BY seq DESC LIMIT 1 OFFSET ?2"
            ),
            rusqlite::params![session_id, max_events as i64],
            |row| row.get(0),
        )
        .optional()
        .unwrap_or(None);
    let Some(cutoff) = cutoff else {
        return;
    };
    for (start, end, id) in BACKGROUND_WORK_EVENTS {
        // Reused ids and different tracking allocations cannot close each other.
        let delete_started = format!(
            "DELETE FROM {events} AS started
             WHERE started.session_id = ?1 AND started.seq <= ?2
               AND started.discriminant = '{start}'
               AND EXISTS (
                   SELECT 1 FROM {events} AS ended
                   WHERE ended.session_id = ?1 AND ended.discriminant = '{end}'
                     AND ended.seq > started.seq
                     AND json_extract(ended.qualification_json, '$.resource_ref')
                         IS json_extract(started.qualification_json, '$.resource_ref')
                     AND json_extract(ended.event_json, '$.{end}.{id}') =
                         json_extract(started.event_json, '$.{start}.{id}'))"
        );
        let delete_ended = format!(
            "DELETE FROM {events}
             WHERE session_id = ?1 AND seq <= ?2 AND discriminant = '{end}'"
        );
        for sql in [delete_started, delete_ended] {
            if let Err(e) = conn.execute(&sql, rusqlite::params![session_id, cutoff]) {
                warn!(target: "acp.event_store", "prune completed work {session_id}: {e}");
                return;
            }
        }
    }
}

fn decode(json: &str) -> Option<Event> {
    serde_json::from_str(json).ok()
}

fn decode_logged(json: &str, session_id: &str, what: &str) -> Option<Event> {
    serde_json::from_str(json)
        .map_err(|e| {
            warn!(
                target: "acp.event_store",
                session = %session_id,
                "{what}: deserialise failed: {e}"
            )
        })
        .ok()
}

/// Log a failed query as `None`.
fn logged<T>(result: rusqlite::Result<T>, what: &str, session_id: &str) -> Option<T> {
    result
        .map_err(|e| warn!(target: "acp.event_store", "{what} for {session_id}: {e}"))
        .ok()
}

/// Run a single-column string query bound to `session_id`; empty on error.
fn query_strings(conn: &Connection, sql: &str, what: &str, session_id: &str) -> Vec<String> {
    let Some(mut stmt) = logged(conn.prepare(sql), what, session_id) else {
        return Vec::new();
    };
    let rows = stmt.query_map(rusqlite::params![session_id], |row| row.get::<_, String>(0));
    logged(rows, what, session_id)
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
}

/// Variant name of a serialized event, for trace breadcrumbs that must not
/// dump payloads. Serde writes `{"Variant":{..}}`, or `"Variant"` for unit ones.
fn event_kind(json: &str) -> &str {
    json.trim_start_matches(['{', '"'])
        .split(['"', ':'])
        .next()
        .unwrap_or_default()
}

#[cfg(test)]
pub(super) mod test_support {
    use super::*;
    use tempfile::TempDir;

    pub(in crate::acp::event_store) fn open_store(max: usize) -> (TempDir, EventStore) {
        let tmp = TempDir::new().unwrap();
        let store = EventStore::open(&tmp.path().join("acp.db"), max).unwrap();
        (tmp, store)
    }

    /// Record `events` at consecutive seqs starting from `first_seq`.
    pub(in crate::acp::event_store) fn record_from(
        store: &EventStore,
        session_id: &str,
        first_seq: u64,
        events: impl IntoIterator<Item = Event>,
    ) {
        for (i, event) in events.into_iter().enumerate() {
            store
                .record(
                    session_id,
                    first_seq + i as u64,
                    &event,
                    &crate::acp::event_sender::EventMetadata::history(),
                )
                .unwrap();
        }
    }

    // Stored projection data only; no runtime Native capability is constructed.
    pub(in crate::acp::event_store) fn record_projection(
        store: &EventStore,
        session_id: &str,
        first_seq: u64,
        events: impl IntoIterator<Item = Event>,
    ) {
        use crate::acp::event_sender::{EventMetadata, EventScope};
        use crate::acp::protocol::{
            AcpOperationMetadata, AcpOriginalRequestResult, AcpPromptObservation, AcpPromptWindowId,
        };
        let origin = crate::acp::state::NativeEventOrigin {
            context: serde_json::from_value(serde_json::to_value([31u8; 32]).unwrap()).unwrap(),
            generation: 1,
            attempt_id: serde_json::from_value(serde_json::json!(uuid::Uuid::new_v4())).unwrap(),
            producer: uuid::Uuid::new_v4(),
            owner: None,
        };
        let mut correlation = crate::acp::establishment::NativeOperationCorrelation::new(None);
        let mut window_id = AcpPromptWindowId::new();
        for (offset, event) in events.into_iter().enumerate() {
            if matches!(event, Event::ThinkingStarted) {
                correlation = crate::acp::establishment::NativeOperationCorrelation::new(None);
                window_id = AcpPromptWindowId::new();
            }
            let failed = matches!(
                event,
                Event::RateLimit { .. } | Event::PromptRuntimeError { .. }
            );
            let metadata = match &event {
                Event::UserPromptSent { .. }
                | Event::UserDiffCommentsPrompt { .. }
                | Event::RateLimitAutoResumed { .. }
                | Event::SessionCleared
                | Event::AgentSwitched { .. }
                | Event::Stopped { .. } => EventMetadata::host(&event),
                Event::AgentStartupError { .. } => EventMetadata {
                    scope: EventScope::SessionCurrent,
                    native_origin: Some(origin),
                    ..EventMetadata::history()
                },
                Event::AcpSessionAssigned { .. } | Event::AcpConnectionUnavailable { .. } => {
                    EventMetadata::history()
                }
                _ => EventMetadata {
                    scope: EventScope::OperationCurrent,
                    native_origin: Some(origin),
                    operation: Some(AcpOperationMetadata {
                        correlation,
                        outcome: failed.then_some(AcpOriginalRequestResult::Failed),
                    }),
                    prompt_observation: Some(AcpPromptObservation::Written { window_id }),
                    ..EventMetadata::history()
                },
            };
            store
                .record(session_id, first_seq + offset as u64, &event, &metadata)
                .unwrap();
        }
    }

    pub(in crate::acp::event_store) fn seqs(events: &[StoredEvent]) -> Vec<u64> {
        events.iter().map(|event| event.seq).collect()
    }

    pub(in crate::acp::event_store) use crate::acp::state::test_support::{
        chunk as agent_chunk, prompt as user_prompt, stopped,
    };

    pub(in crate::acp::event_store) fn tool_call(id: &str) -> crate::acp::state::ToolCall {
        crate::acp::state::ToolCall {
            id: id.into(),
            name: "Bash".into(),
            kind: "execute".into(),
            args_preview: "ls".into(),
            started_at: chrono::Utc::now(),
            parent_tool_call_id: None,
            memory_recall: None,
            diffs: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn every_non_substantive_event_is_retention_pinned() {
        for name in NON_SUBSTANTIVE_EVENT_DISCRIMINANTS {
            assert!(
                RETENTION_PINNED_EVENT_DISCRIMINANTS.contains(name),
                "{name}"
            );
        }
    }

    #[test]
    fn record_is_idempotent_and_persisted_shapes_stay_readable() {
        let (tmp, store) = open_store(1000);
        store
            .record(
                "s-1",
                1,
                &user_prompt("hi"),
                &crate::acp::event_sender::EventMetadata::history(),
            )
            .unwrap();
        store
            .record("s-1", 1, &user_prompt("hi"), &EventMetadata::history())
            .unwrap();
        assert!(store
            .record("s-1", 1, &Event::ThinkingStarted, &EventMetadata::history())
            .is_err());
        let mut legacy = EventMetadata::history();
        legacy.scope = EventScope::LegacyUnqualified;
        assert!(store.record("s-1", 1, &user_prompt("hi"), &legacy).is_err());
        let mut qualified = EventMetadata::history();
        qualified.operation = Some(crate::acp::protocol::AcpOperationMetadata {
            correlation: crate::acp::establishment::NativeOperationCorrelation::new(None),
            outcome: None,
        });
        assert!(store
            .record("s-1", 1, &user_prompt("hi"), &qualified)
            .is_err());
        store
            .record(
                "s-1",
                2,
                &agent_chunk("hi back"),
                &crate::acp::event_sender::EventMetadata::history(),
            )
            .unwrap();
        store
            .record(
                "s-2",
                1,
                &Event::ThinkingStarted,
                &crate::acp::event_sender::EventMetadata::history(),
            )
            .unwrap();
        drop(store);

        let store = EventStore::open(&tmp.path().join("acp.db"), 1000).unwrap();
        let replay = store.replay_from("s-1", 0).unwrap();
        assert_eq!(seqs(&replay), [1, 2]);
        assert!(
            matches!(replay[0].event.as_ref(), Event::UserPromptSent { text, .. } if text == "hi"),
            "conflicting appends leave the original payload unchanged"
        );
        assert_eq!(replay[0].metadata, EventMetadata::history());
        let mut listed = store.all_session_seqs().unwrap();
        listed.sort();
        assert_eq!(listed, [("s-1".to_string(), 2), ("s-2".to_string(), 1)]);
        assert_eq!(store.lowest_seq("s-1"), Some(1));

        store.delete_session("s-1");
        assert_eq!(store.highest_seq("s-1"), 0);
        assert_eq!(store.lowest_seq("s-1"), None);
        assert_eq!(store.highest_seq("s-2"), 1, "siblings are untouched");

        // Persisted prompt shapes, legacy and current, stay readable.
        use crate::acp::state::DiffComment;
        let legacy: Event =
            serde_json::from_str(r#"{"UserPromptSent":{"text":"legacy"}}"#).expect("legacy event");
        assert!(
            matches!(legacy, Event::UserPromptSent { ref text, ref attachments, .. } if text == "legacy" && attachments.is_empty())
        );

        let comment = |repo_name: Option<&str>| DiffComment {
            id: "c-1".into(),
            repo_name: repo_name.map(Into::into),
            file_path: "src/main.rs".into(),
            side: "new".into(),
            start_line: 42,
            end_line: 45,
            body: "rename this".into(),
            captured_snippet: "fn main() {}".into(),
            language: Some("rust".into()),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: None,
        };
        let prompt = |repo_name| Event::UserDiffCommentsPrompt {
            intro: "Hey:".into(),
            outro: "Please address these comments.".into(),
            is_multi_repo: true,
            comments: vec![comment(repo_name)],
            assembled_markdown: "## Diff comments\n\n...".into(),
        };
        let json = serde_json::to_string(&prompt(None)).unwrap();
        for key in [
            "\"isMultiRepo\"",
            "\"assembledMarkdown\"",
            "\"filePath\"",
            "\"startLine\"",
        ] {
            assert!(json.contains(key), "{key} missing from {json}");
        }
        assert!(!json.contains("\"repoName\"") && !json.contains("\"updatedAt\""));

        store
            .record(
                "shapes",
                1,
                &prompt(Some("repoA")),
                &crate::acp::event_sender::EventMetadata::history(),
            )
            .unwrap();
        match store.replay_from("shapes", 0).unwrap()[0].event.as_ref() {
            Event::UserDiffCommentsPrompt {
                intro,
                is_multi_repo,
                comments,
                assembled_markdown,
                ..
            } => {
                assert_eq!(intro, "Hey:");
                assert!(*is_multi_repo);
                assert_eq!(comments[0].repo_name.as_deref(), Some("repoA"));
                assert_eq!(comments[0].start_line, 42);
                assert!(assembled_markdown.starts_with("## Diff comments"));
            }
            other => panic!("expected UserDiffCommentsPrompt, got {other:?}"),
        }
    }

    #[test]
    fn retention_keeps_starts_until_their_own_allocation_ends() {
        let a = uuid::Uuid::from_u128(1).to_string();
        let b = uuid::Uuid::from_u128(2).to_string();
        for (start_ref, end_ref, closed) in [
            (Some(a.as_str()), None, false),
            (Some(a.as_str()), Some(b.as_str()), false),
            (Some(a.as_str()), Some(a.as_str()), true),
            (None, None, true),
        ] {
            let (_tmp, store) = open_store(3);
            let conn = store.conn();
            let start = Event::BackgroundTaskStarted {
                task_id: "reused".into(),
                tool_call_id: None,
                description: "tracked work".into(),
                workflow: None,
            };
            let end = Event::BackgroundTaskFinished {
                task_id: "reused".into(),
                ended_at: None,
            };
            for (seq, event, reference) in [(1, start, start_ref), (2, end, end_ref)] {
                let json = serde_json::to_string(&event).unwrap();
                let qualification = reference
                    .map(|reference| serde_json::json!({ "resource_ref": reference }).to_string());
                conn.execute(
                    "INSERT INTO acp_events
                     (session_id, seq, event_json, created_at, discriminant, scope, qualification_json)
                     VALUES ('s', ?1, ?2, 0, ?3, ?4, ?5)",
                    rusqlite::params![seq, json, event_kind(&json),
                        if reference.is_some() { 1 } else { 0 }, qualification],
                ).unwrap();
            }
            for seq in 3..=8 {
                conn.execute(
                    "INSERT INTO acp_events
                     (session_id, seq, event_json, created_at, discriminant, scope, qualification_json)
                     VALUES ('s', ?1, '\"ThinkingStarted\"', 0, 'ThinkingStarted', 1, NULL)",
                    [seq],
                ).unwrap();
            }
            prune_completed_background_work(&conn, &store.schema, "s", 3);
            let starts: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM acp_events WHERE session_id = 's'
                 AND discriminant = 'BackgroundTaskStarted'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(starts, i64::from(!closed), "{start_ref:?} / {end_ref:?}");
        }
    }

    #[test]
    fn retention_drops_the_oldest_but_keeps_snapshot_events() {
        let (_tmp, store) = open_store(3);
        record_from(&store, "plain", 1, (1..=5).map(|_| Event::ThinkingStarted));
        assert_eq!(seqs(&store.replay_from("plain", 0).unwrap()), [3, 4, 5]);

        let snapshots = [
            Event::AvailableCommandsUpdated { commands: vec![] },
            Event::ModesAvailable {
                current_mode_id: "default".into(),
                modes: vec![],
            },
            Event::AcpSessionAssigned {
                acp_session_id: "acp-xyz".into(),
                connection_ready: false,
                native_subject: None,
            },
        ];
        record_from(&store, "s-1", 1, snapshots);
        record_from(&store, "s-1", 4, (4..=20).map(|_| Event::ThinkingStarted));
        let kept = seqs(&store.replay_from("s-1", 0).unwrap());
        for seq in [1, 2, 3, 20] {
            assert!(kept.contains(&seq), "seq {seq} dropped: {kept:?}");
        }
        assert!(
            !kept.contains(&5),
            "stale transcript event leaked: {kept:?}"
        );
        assert!(store.lowest_seq("plain").unwrap() > 1);

        // The prune matches snapshot rows by their serialized prefix.
        let cases = [
            (
                Event::AvailableCommandsUpdated { commands: vec![] },
                "AvailableCommandsUpdated",
            ),
            (
                Event::CurrentModeChanged {
                    current_mode_id: "default".into(),
                },
                "CurrentModeChanged",
            ),
        ];
        for (event, name) in cases {
            let json = serde_json::to_string(&event).unwrap();
            assert!(json.starts_with(&format!("{{\"{name}\":")), "{json}");
        }
    }
}
