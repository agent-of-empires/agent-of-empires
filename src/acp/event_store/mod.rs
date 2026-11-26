//! Disk-backed event log for structured view sessions.

// Management admission does not end work retained by the native runner. Keep
// this predicate identical across turn, responder, monitor and budget queries.
macro_rules! native_channel_loss_sql {
    ($json:literal) => {
        concat!(
            "(CASE WHEN discriminant = 'AcpConnectionUnavailable' THEN json_extract(",
            $json,
            ", '$.AcpConnectionUnavailable.source') IN ('sdk','control','native')",
            " AND json_type(",
            $json,
            ", '$.AcpConnectionUnavailable.native_subject.kind.AdmissionRefusal') IS NULL ELSE 0 END)"
        )
    };
}
mod account_limits;
mod attachments;
pub(crate) mod control;
mod establishment;
pub(crate) mod identity;
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

pub use account_limits::{AccountState, WindowLimit};
pub use attachments::AttachmentBlob;
pub use control::ControlKnowledge;
pub(crate) use establishment::EstablishmentSeal;
pub use establishment::ReplayCommit;
pub use rate_limit::{RateLimitPark, ResetProvenance};
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

/// Selects the shell starts among task starts (`recent` is the start row).
const SHELL_START: &str =
    "json_extract(recent.event_json, '$.BackgroundTaskStarted.workflow') IS NULL";
const WORKFLOW_START: &str =
    "json_extract(recent.event_json, '$.BackgroundTaskStarted.workflow') IS NOT NULL";

/// Background work as its (start, end, id field, kept groups) events. Teardown
/// and cold folds find open work by pairing them, so the retention prune pins
/// both halves, and [`prune_completed_background_work`] drops a pair once it
/// ends, except the newest ended starts of each kept group, so the shell panel
/// survives a reload and workflows cannot displace shells.
const BACKGROUND_WORK_EVENTS: &[(&str, &str, &str, &[&str])] = &[
    (
        "BackgroundTaskStarted",
        "BackgroundTaskFinished",
        "task_id",
        &[SHELL_START, WORKFLOW_START],
    ),
    (
        "BackgroundAgentLaunched",
        "BackgroundAgentCompleted",
        "agent_id",
        &[],
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
    "SubagentUpdated",
];

/// Idle read connections kept open; concurrent reads beyond this open more.
const MAX_IDLE_READERS: usize = 4;

/// SQLite-backed structured view event log.
pub struct EventStore {
    conn: Mutex<Connection>,
    /// Independent file-backed reader; private databases share `conn`.
    read_conn: Option<Mutex<Connection>>,
    readers: Mutex<Vec<Connection>>,
    db_path: std::path::PathBuf,
    schema: events::Schema,
    max_events_per_session: usize,
    control_cache: Arc<super::control_cache::ControlStateCache>,
    #[cfg(test)]
    retained_read_gate: Mutex<Option<test_support::RetainedReadGate>>,
    #[cfg(any(test, debug_assertions))]
    _temp_directory: Option<tempfile::TempDir>,
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

fn open_reader(db_path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("open read-only handle at {}", db_path.display()))?;
    conn.pragma_update(None, "query_only", "ON")
        .context("set query_only=ON on read handle")?;
    conn.busy_timeout(std::time::Duration::from_millis(1000))
        .context("set busy_timeout on read handle")?;
    Ok(conn)
}

/// A pooled read connection, or the writer's when none could be opened.
enum ReadConn<'a> {
    Pooled(&'a EventStore, Option<Connection>),
    Writer(MutexGuard<'a, Connection>),
}

impl std::ops::Deref for ReadConn<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        match self {
            Self::Pooled(_, conn) => conn.as_ref().expect("held until drop"),
            Self::Writer(guard) => guard,
        }
    }
}

impl Drop for ReadConn<'_> {
    fn drop(&mut self) {
        if let Self::Pooled(store, conn) = self {
            let mut idle = store.readers.lock().unwrap_or_else(|p| p.into_inner());
            if idle.len() < MAX_IDLE_READERS {
                idle.extend(conn.take());
            }
        }
    }
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

    /// Open or create the database using its sole control-state cache.
    pub fn open(
        db_path: &Path,
        max_events_per_session: usize,
        control_cache: Arc<super::control_cache::ControlStateCache>,
    ) -> Result<Self> {
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
                Self::validate_autonomous_commit_schema(&conn)?;
                Self::validate_budget_knowledge(&conn)?;
                events::initialize_schema(&conn, &schema)?;
            }
            EventTableShape::Legacy { .. } => {
                anyhow::bail!("ACP event database requires migration59 before opening")
            }
        }
        identity::install(&mut conn)?;
        account_limits::ensure_schema(&conn).context("create account limits table")?;
        let private_database: bool = conn.query_row(
            "SELECT file = '' FROM pragma_database_list WHERE name = 'main'",
            [],
            |row| row.get(0),
        )?;
        let read_conn = if private_database {
            None
        } else {
            Some(Mutex::new(open_reader(db_path)?))
        };
        debug!(
            target: "acp.event_store",
            path = %db_path.display(),
            cap = max_events_per_session,
            "structured view event store opened"
        );
        Ok(Self {
            conn: Mutex::new(conn),
            read_conn,
            readers: Mutex::new(Vec::new()),
            db_path: db_path.to_path_buf(),
            schema,
            max_events_per_session,
            control_cache,
            #[cfg(test)]
            retained_read_gate: Mutex::new(None),
            #[cfg(any(test, debug_assertions))]
            _temp_directory: None,
        })
    }

    pub fn control_cache(&self) -> &Arc<super::control_cache::ControlStateCache> {
        &self.control_cache
    }

    #[cfg(any(test, debug_assertions))]
    pub(crate) fn temporary_for_test(max_events_per_session: usize) -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let mut store = Self::open(
            &directory.path().join("acp_events.db"),
            max_events_per_session,
            Default::default(),
        )?;
        store._temp_directory = Some(directory);
        Ok(store)
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    #[cfg(test)]
    pub(crate) fn test_conn(&self) -> MutexGuard<'_, Connection> {
        self.conn()
    }

    fn read_conn(&self) -> MutexGuard<'_, Connection> {
        self.read_conn
            .as_ref()
            .unwrap_or(&self.conn)
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }
    /// A connection for read-only queries that does not wait on writes.
    fn reader(&self) -> ReadConn<'_> {
        if self.read_conn.is_none() {
            return ReadConn::Writer(self.conn());
        }
        let idle = self.readers.lock().unwrap_or_else(|p| p.into_inner()).pop();
        match idle.map_or_else(|| open_reader(&self.db_path), Ok) {
            Ok(conn) => ReadConn::Pooled(self, Some(conn)),
            Err(e) => {
                warn!(target: "acp.event_store", "read handle unavailable; reading on the writer: {e:#}");
                ReadConn::Writer(self.conn())
            }
        }
    }

    /// Make every query on the event table fail, or restore it.
    #[cfg(test)]
    pub(crate) fn test_set_unavailable(&self, unavailable: bool) {
        let (from, to) = if unavailable {
            ("acp_events", "acp_events_offline")
        } else {
            ("acp_events_offline", "acp_events")
        };
        self.conn()
            .execute_batch(&format!("ALTER TABLE {from} RENAME TO {to}"))
            .unwrap();
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
        let json = Self::encode_event(event)
            .with_context(|| format!("serialise event for {session_id}@{seq}"))?;
        self.record_json(
            session_id,
            seq,
            event,
            metadata,
            crate::time::Timestamp::now().as_millis(),
            json,
        )
    }

    /// Append a `RateLimit` whose reset was copied from an earlier event, stamped with when that
    /// reset was originally reported and the window it was reported for. Only an inherited reset
    /// carries the stamp; any other `RateLimit` is observed when it is recorded.
    pub(crate) fn record_inherited_rate_limit(
        &self,
        session_id: &str,
        seq: u64,
        event: &Event,
        metadata: &EventMetadata,
        observed_at_ms: i64,
        source_window: &str,
    ) -> Result<RecordDisposition> {
        let json = inherited_json(session_id, seq, event, observed_at_ms, source_window)?;
        self.record_json(
            session_id,
            seq,
            event,
            metadata,
            crate::time::Timestamp::now().as_millis(),
            json,
        )
    }

    /// Record qualified projection test data at an explicit observation time.
    #[cfg(test)]
    pub(crate) fn record_quota_calendar_at(
        &self,
        session_id: &str,
        seq: u64,
        event: &Event,
        metadata: &EventMetadata,
        created_at_ms: i64,
        inherited: Option<(i64, &str)>,
    ) -> Result<RecordDisposition> {
        let json = match inherited {
            Some((observed_at_ms, source_window)) => {
                inherited_json(session_id, seq, event, observed_at_ms, source_window)?
            }
            None => Self::encode_event(event)?,
        };
        self.record_json(session_id, seq, event, metadata, created_at_ms, json)
    }

    fn record_json(
        &self,
        session_id: &str,
        seq: u64,
        event: &Event,
        metadata: &EventMetadata,
        now_ms: i64,
        json: String,
    ) -> Result<RecordDisposition> {
        metadata.validate_for_event(event)?;
        anyhow::ensure!(
            metadata.clear_after_seq.is_none_or(|cut| cut < seq),
            "conversation display cut must precede its clear"
        );
        let recorded = self.append_controlled(session_id, seq, event, metadata, now_ms, |tx| {
            if let Some(recorded) =
                Self::record_autonomous_result(tx, session_id, seq, &json, now_ms, metadata)?
            {
                return Ok(recorded);
            }
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
    /// Hold the writer connection, as a slow save does.
    #[cfg(test)]
    pub(crate) fn hold_writer(&self) -> MutexGuard<'_, Connection> {
        self.conn()
    }

    #[cfg(test)]
    pub(crate) fn record_at(
        &self,
        session_id: &str,
        seq: u64,
        event: &Event,
        created_at_ms: i64,
    ) -> Result<()> {
        let json = Self::encode_event(event)?;
        self.record_json_at(session_id, seq, &json, created_at_ms)
    }

    #[cfg(test)]
    fn record_json_at(
        &self,
        session_id: &str,
        seq: u64,
        json: &str,
        created_at_ms: i64,
    ) -> Result<()> {
        self.control_cache
            .with_control(session_id, |cursor| -> Result<()> {
                events::insert_event(
                    &self.conn(),
                    &self.schema,
                    session_id,
                    seq,
                    json,
                    created_at_ms,
                )?;
                *cursor = None;
                Ok(())
            })?;
        self.control_cache.forget(session_id);
        Ok(())
    }

    /// Highest actual event or retained publication sequence; malformed facts refuse.
    pub fn highest_seq(&self, session_id: &str) -> Result<u64> {
        let conn = self.reader();
        let tx = conn.unchecked_transaction()?;
        let max = Self::highest_seq_in_snapshot(&tx, session_id)?;
        tx.commit()?;
        trace!(
            target: "acp.event_store",
            session = %session_id,
            highest_seq = max,
            "highest_seq query"
        );
        Ok(max)
    }

    /// Lowest seq still stored for `session_id`, or `None` when nothing is.
    pub fn lowest_seq(&self, session_id: &str) -> Option<u64> {
        let min = events::lowest_seq(&self.reader(), &self.schema, session_id);
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
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        Self::validate_prompt_commit_schema(&tx)?;
        Self::validate_autonomous_commit_schema(&tx)?;
        let mut statement = tx.prepare(
            "SELECT session_id, MAX(seq) FROM (
                SELECT session_id, MAX(seq) AS seq FROM acp_events GROUP BY session_id
                UNION ALL
                SELECT session_id, MAX(
                    COALESCE(json_extract(prompt_commit_json, '$.diagnostic.seq'), 0),
                    COALESCE(json_extract(prompt_commit_json, '$.stopped'), 0))
                FROM acp_establishment_commit WHERE prompt_commit_json IS NOT NULL
                UNION ALL
                SELECT session_id, MAX(
                    COALESCE(json_extract(autonomous_commit_json, '$.started'), 0),
                    COALESCE(json_extract(autonomous_commit_json, '$.ended'), 0),
                    COALESCE(json_extract(autonomous_commit_json, '$.boundary.seq'), 0))
                FROM acp_establishment_commit WHERE autonomous_commit_json IS NOT NULL
             ) GROUP BY session_id",
        )?;
        let mut rows = statement.query([])?;
        let mut collected = Vec::new();
        while let Some(row) = rows.next()? {
            let seq = u64::try_from(row.get::<_, i64>(1)?)
                .context("negative ACP publication sequence")?;
            collected.push((row.get::<_, String>(0)?, seq));
        }
        drop(rows);
        drop(statement);
        tx.commit()?;
        debug!(
            target: "acp.event_store",
            sessions = collected.len(),
            "all_session_seqs hydration"
        );
        Ok(collected)
    }

    pub(super) fn highest_seq_in_snapshot(
        conn: &rusqlite::Connection,
        session_id: &str,
    ) -> Result<u64> {
        let seq: i64 = conn.query_row(
            "SELECT COALESCE(MAX(seq),0) FROM acp_events WHERE session_id=?1",
            rusqlite::params![session_id],
            |row| row.get(0),
        )?;
        let (retained, _) = Self::retained_publication_state(conn, session_id)?;
        Ok(u64::try_from(seq)?.max(retained))
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
    for (start, end, id, kept_groups) in BACKGROUND_WORK_EVENTS {
        // Reused ids and different tracking allocations cannot close each other.
        let closed = |started: &str| {
            format!(
                "EXISTS (
                   SELECT 1 FROM {events} AS ended
                   WHERE ended.session_id = ?1 AND ended.discriminant = '{end}'
                     AND ended.seq > {started}.seq
                     AND json_extract(ended.qualification_json, '$.resource_ref')
                         IS json_extract({started}.qualification_json, '$.resource_ref')
                     AND json_extract(ended.event_json, '$.{end}.{id}') =
                         json_extract({started}.event_json, '$.{start}.{id}'))"
            )
        };
        // Open starts are pinned, so they do not count toward the kept ones.
        let keep = crate::acp::state::MAX_FINISHED_BACKGROUND_SHELLS;
        let kept: String = kept_groups
            .iter()
            .map(|group| {
                format!(
                    "AND started.seq NOT IN (
                       SELECT recent.seq FROM {events} AS recent
                       WHERE recent.session_id = ?1 AND recent.discriminant = '{start}'
                         AND {group} AND {}
                       ORDER BY recent.seq DESC LIMIT {keep})",
                    closed("recent")
                )
            })
            .collect::<Vec<_>>()
            .join("\n               ");
        let delete_started = format!(
            "DELETE FROM {events} AS started
             WHERE started.session_id = ?1 AND started.seq <= ?2
               AND started.discriminant = '{start}'
               {kept}
               AND {}",
            closed("started")
        );
        let delete_ended = format!(
            "DELETE FROM {events} AS ended
             WHERE ended.session_id = ?1 AND ended.seq <= ?2
               AND ended.discriminant = '{end}'
               AND NOT EXISTS (
                   SELECT 1 FROM {events} AS started
                   WHERE started.session_id = ?1 AND started.discriminant = '{start}'
                     AND started.seq < ended.seq
                     AND json_extract(ended.event_json, '$.{end}.{id}') =
                         json_extract(started.event_json, '$.{start}.{id}'))"
        );
        for sql in [delete_started, delete_ended] {
            if let Err(e) = conn.execute(&sql, rusqlite::params![session_id, cutoff]) {
                warn!(target: "acp.event_store", "prune completed work {session_id}: {e}");
                return;
            }
        }
    }
}

/// A `RateLimit`'s JSON stamped as inheriting its reset, see `record_inherited_rate_limit`.
fn inherited_json(
    session_id: &str,
    seq: u64,
    event: &Event,
    observed_at_ms: i64,
    source_window: &str,
) -> Result<String> {
    let mut value = serde_json::to_value(event)
        .with_context(|| format!("serialise event for {session_id}@{seq}"))?;
    let object = value
        .get_mut("RateLimit")
        .and_then(serde_json::Value::as_object_mut)
        .context("inherited reset on an event that is not a RateLimit")?;
    object.insert(
        rate_limit::INHERITED_OBSERVATION_KEY.into(),
        observed_at_ms.into(),
    );
    object.insert(
        rate_limit::INHERITED_WINDOW_KEY.into(),
        source_window.into(),
    );
    Ok(value.to_string())
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

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(in crate::acp::event_store) enum RetainedReadStage {
        Snapshot,
        BeforeDecode,
    }

    pub(in crate::acp::event_store) struct RetainedReadGate {
        session: String,
        stage: RetainedReadStage,
        reached: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }

    pub(in crate::acp::event_store) fn gate_retained_read(
        store: &EventStore,
        session: &str,
        stage: RetainedReadStage,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (reached, entered) = std::sync::mpsc::channel();
        let (release, resume) = std::sync::mpsc::channel();
        let mut slot = store.retained_read_gate.lock().unwrap();
        assert!(slot.is_none());
        *slot = Some(RetainedReadGate {
            session: session.into(),
            stage,
            reached,
            release: resume,
        });
        (entered, release)
    }

    impl EventStore {
        pub(in crate::acp::event_store) fn pause_retained_read(
            &self,
            session: &str,
            stage: RetainedReadStage,
        ) {
            let gate = {
                let mut slot = self.retained_read_gate.lock().unwrap();
                if slot
                    .as_ref()
                    .is_some_and(|gate| gate.session == session && gate.stage == stage)
                {
                    slot.take()
                } else {
                    None
                }
            };
            if let Some(gate) = gate {
                gate.reached.send(()).unwrap();
                let _ = gate.release.recv();
            }
        }
    }

    pub(in crate::acp::event_store) fn open_store(max: usize) -> (TempDir, EventStore) {
        let tmp = TempDir::new().unwrap();
        let store = EventStore::open(&tmp.path().join("acp.db"), max, Default::default()).unwrap();
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
            prompt: None,
            id: id.into(),
            name: "Bash".into(),
            kind: "execute".into(),
            args_preview: "ls".into(),
            started_at: crate::time::Timestamp::now(),
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

        let store = EventStore::open(&tmp.path().join("acp.db"), 1000, Default::default()).unwrap();
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
        assert_eq!(store.highest_seq("s-1").unwrap(), 0);
        assert_eq!(store.lowest_seq("s-1"), None);
        assert_eq!(
            store.highest_seq("s-2").unwrap(),
            1,
            "siblings are untouched"
        );

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
                started_at: None,
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
            for index in 0..24 {
                let task_id = format!("newer-{index}");
                for (seq, event) in [
                    (
                        3 + index * 2,
                        Event::BackgroundTaskStarted {
                            task_id: task_id.clone(),
                            tool_call_id: None,
                            description: "newer completed work".into(),
                            workflow: None,
                            started_at: None,
                        },
                    ),
                    (
                        4 + index * 2,
                        Event::BackgroundTaskFinished {
                            task_id,
                            ended_at: None,
                        },
                    ),
                ] {
                    let json = serde_json::to_string(&event).unwrap();
                    conn.execute(
                        "INSERT INTO acp_events
                         (session_id, seq, event_json, created_at, discriminant, scope, qualification_json)
                         VALUES ('s', ?1, ?2, 0, ?3, 0, NULL)",
                        rusqlite::params![seq, json, event_kind(&json)],
                    )
                    .unwrap();
                }
            }
            for seq in 51..=56 {
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
                 AND discriminant = 'BackgroundTaskStarted'
                 AND json_extract(event_json, '$.BackgroundTaskStarted.task_id') = 'reused'",
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

    #[tokio::test]
    #[serial_test::serial]
    async fn native_autonomous_fact_survives_pruning_and_refuses_conflicting_replay() {
        use crate::acp::event_store::ControlKnowledge::Known;
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| Box::pin(async move {
            let mut live = crate::server::test_support::managed_control_session(scope, "retained-autonomous-fact", "claude", None).await;
            let id = live.id().to_owned();
            let run = uuid::Uuid::new_v4();
            live.native.send(serde_json::json!({"jsonrpc":"2.0","method":"_omp/autonomous_turn/start",
                "params":{"version":1,"sessionId":live.sid,"runId":run,"source":"irc"}})).await;
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while !matches!(live.client.next_event().await, Some(Event::ThinkingStarted)) {}
            }).await.expect("the genuine original start reaches its publication consumer");
            let store = Arc::clone(&live.state.acp_event_store);
            let start = store.replay_from(&id, 0).unwrap().into_iter().find(|stored|
                stored.metadata.native_autonomous.as_ref().is_some_and(|autonomous|
                    autonomous.identity.run_id == run)).expect("actual committed original start");
            assert!(start.metadata.operation.is_none() && start.metadata.native_prompt_id.is_none());
            let frame = crate::acp::protocol::AcpBroadcastFrame::from_qualified(
                id.clone(), start.seq, Arc::clone(&start.event), &start.metadata, None, None, None);
            let public = serde_json::to_value(&frame).unwrap();
            assert!(public.get("native_autonomous").is_none());
            assert!(public.get("native_origin").is_none());
            assert!(!serde_json::to_string(&public).unwrap().contains(&run.to_string()));
            assert!(frame.native_subject.is_none());
            live.native.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                "sessionId":live.sid,"update":{"sessionUpdate":"agent_message_chunk",
                    "content":{"type":"text","text":"past the original start"}}}})).await;
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while !matches!(live.client.next_event().await,
                    Some(Event::AgentMessageChunk { text, .. }) if text == "past the original start") {}
            }).await.expect("real Native progress provides the retention cutoff");
            {
                let conn = store.conn();
                crate::events::prune_retention(&conn, &store.schema, &id, 1, RETENTION_PINNED_EVENT_DISCRIMINANTS);
            }
            assert!(!store.replay_from(&id, 0).unwrap().iter().any(|stored| stored.seq == start.seq));
            store.control_cache.forget(&id);
            assert_eq!(store.holds_live_work(&id).unwrap(), Known(true),
                "cold control hydration must retain the actual admitted original");
            let page = store.replay_page(&id, 0, None).unwrap();
            let mut projected = crate::acp::state::AcpState::new(
                crate::acp::state::AcpSessionId(id.clone()),
                crate::acp::state::AgentName("claude".into()), None);
            page.autonomous.as_ref().expect("same-snapshot retained fact").restore_state(&mut projected);
            assert!(projected.native_autonomous_active && !projected.turn_active);
            let frontier = store.highest_seq(&id).unwrap();
            assert_eq!(page.highest_seq, frontier);
            assert_eq!(store.record(&id, frontier + 1, &start.event, &start.metadata).unwrap(),
                RecordDisposition::Existing { seq: start.seq });
            assert_eq!(store.highest_seq(&id).unwrap(), frontier);
            let mut conflicting = start.metadata.clone();
            let autonomous = Arc::make_mut(conflicting.native_autonomous.as_mut().unwrap());
            Arc::make_mut(&mut autonomous.identity).run_id = uuid::Uuid::new_v4();
            assert!(store.record(&id, frontier + 1, &start.event, &conflicting).is_err());
            assert_eq!(store.highest_seq(&id).unwrap(), frontier);
            assert_eq!(store.holds_live_work(&id).unwrap(), Known(true));
            let retained: String = store.conn().query_row(
                "SELECT autonomous_commit_json FROM acp_establishment_commit WHERE session_id=?1",
                [&id], |row| row.get(0)).unwrap();
            store.conn().execute(
                "UPDATE acp_establishment_commit SET autonomous_commit_json='{' WHERE session_id=?1",
                [&id]).unwrap();
            let refused = (
                store.highest_seq(&id).is_err(),
                store.replay_page(&id, 0, None).is_err(),
                store.all_session_seqs().is_err(),
                store.holds_live_work(&id).is_err(),
            );
            store.conn().execute(
                "UPDATE acp_establishment_commit SET autonomous_commit_json=?1 WHERE session_id=?2",
                rusqlite::params![retained, id]).unwrap();
            assert_eq!(refused, (true, true, true, true),
                "malformed retained metadata cannot certify an empty or low-water view");
            live.native.send(serde_json::json!({"jsonrpc":"2.0","method":"_omp/autonomous_turn/end",
                "params":{"version":1,"sessionId":live.sid,"runId":run,"source":"irc","stopReason":"end_turn"}})).await;
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while !matches!(live.client.next_event().await,
                    Some(Event::Stopped { reason }) if reason == "agent_idle") {}
            }).await.expect("only the actual matching terminal closes the retained original");
            let terminal = store.replay_from(&id, 0).unwrap().into_iter().find(|stored|
                stored.metadata.native_autonomous.as_ref().is_some_and(|autonomous|
                    autonomous.phase == crate::acp::control_protocol::NativeAutonomousPhase::Ended))
                .expect("real original terminal");
            let completed = store.replay_page(&id, 0, None).unwrap();
            assert_eq!(completed.autonomous.as_ref().unwrap().activity(), Known(false));
            let unrelated_main = store.holds_live_work(&id).unwrap();
            assert_eq!(unrelated_main, crate::acp::event_store::ControlKnowledge::Indeterminate,
                "an autonomous terminal cannot manufacture main-idle knowledge after retention");
            let frontier = store.highest_seq(&id).unwrap();
            assert_eq!(store.record(&id, frontier + 1, &terminal.event, &terminal.metadata).unwrap(),
                RecordDisposition::Existing { seq: terminal.seq });
            assert_eq!(store.record(&id, frontier + 1, &start.event, &start.metadata).unwrap(),
                RecordDisposition::Existing { seq: start.seq });
            assert_eq!(store.highest_seq(&id).unwrap(), frontier);
            store.control_cache.forget(&id);
            let completed = store.replay_page(&id, 0, None).unwrap();
            assert_eq!(completed.autonomous.as_ref().unwrap().activity(), Known(false),
                "neither a retained start nor a duplicate terminal may reopen completed work");
            assert_eq!(store.holds_live_work(&id).unwrap(), unrelated_main,
                "duplicate autonomous phases must preserve independently unknown main knowledge");
        })).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_autonomous_terminal_keeps_an_independent_written_main_turn_open() {
        use crate::acp::event_store::ControlKnowledge::Known;
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| Box::pin(async move {
            let mut live = crate::server::test_support::managed_control_session(scope, "independent-native-turns", "claude", None).await;
            let id = live.id().to_owned();
            let run = uuid::Uuid::new_v4();
            live.native.send(serde_json::json!({"jsonrpc":"2.0","method":"_omp/autonomous_turn/start",
                "params":{"version":1,"sessionId":live.sid,"runId":run,"source":"irc"}})).await;
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while !matches!(live.client.next_event().await, Some(Event::ThinkingStarted)) {}
            }).await.expect("the actual original's autonomous admission is committed");
            let main_request = live.start_prompt("an independently written main input").await;
            let store = Arc::clone(&live.state.acp_event_store);
            let fold = || {
                let mut state = crate::acp::state::AcpState::new(
                    crate::acp::state::AcpSessionId(id.clone()),
                    crate::acp::state::AgentName("claude".into()), None);
                for stored in store.replay_from(&id, 0).unwrap() {
                    state.apply_scoped_event(stored.seq, &stored.event, &stored.metadata).unwrap();
                }
                state
            };
            let both = fold();
            assert!(both.turn_active && both.native_autonomous_active,
                "the actual Native request and autonomous run have independent live holds");
            assert!(store.replay_from(&id, 0).unwrap().iter().any(|stored|
                matches!(stored.event.as_ref(), Event::ThinkingStarted) && stored.metadata.operation.is_some()));
            let mut historical = both.clone();
            historical.apply_scoped_event(historical.last_seq + 1,
                &Event::SessionCleared, &EventMetadata::history()).unwrap();
            assert!(historical.turn_active && historical.native_autonomous_active,
                "raw historical clear cannot retire either authenticated current owner");
            live.native.send(serde_json::json!({"jsonrpc":"2.0","method":"_omp/autonomous_turn/end",
                "params":{"version":1,"sessionId":live.sid,"runId":run,"source":"irc","stopReason":"end_turn"}})).await;
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while !matches!(live.client.next_event().await,
                    Some(Event::Stopped { reason }) if reason == "agent_idle") {}
            }).await.expect("the actual autonomous terminal is independently consumed");
            let ended = fold();
            assert!(ended.turn_active && !ended.native_autonomous_active,
                "the autonomous terminal cannot complete the genuinely written main request");
            assert_eq!(store.holds_live_work(&id).unwrap(), Known(true));
            let terminal = store.replay_from(&id, 0).unwrap().into_iter().find(|stored|
                stored.metadata.native_autonomous.as_ref().is_some_and(|autonomous|
                    autonomous.phase == crate::acp::control_protocol::NativeAutonomousPhase::Ended))
                .expect("actual private original terminal");
            assert!(terminal.metadata.operation.is_none() && terminal.metadata.native_prompt_id.is_none());
            let mut unmatched = crate::acp::operation_effect::OperationEffects::default();
            assert_eq!(unmatched.apply(crate::acp::operation_effect::ControlKind::Stopped("agent_idle"),
                &terminal.metadata), crate::acp::operation_effect::ControlEffect::Indeterminate);
            assert!(!unmatched.autonomous_open(), "a terminal alone is never positive admission");
            live.finish_prompt(&main_request, None).await;
            let settled = fold();
            assert!(!settled.turn_active && !settled.native_autonomous_active);
            assert_eq!(store.holds_live_work(&id).unwrap(), Known(false));
            let methods = live.native.requests();
            assert_eq!(methods.iter().filter(|method| method.as_str() == "session/new").count(), 1);
            assert_eq!(methods.iter().filter(|method| method.as_str() == "session/prompt").count(), 1);
            assert!(!methods.iter().any(|method| method == "session/load" || method == "session/cancel"));
        })).await;
    }
}
