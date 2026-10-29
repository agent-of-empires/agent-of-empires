//! Disk-backed event log for structured view sessions.

mod attachments;
mod rate_limit;
mod replay;
mod search;
mod turns;
mod wakeups;

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use tracing::{debug, trace, warn};

use super::state::Event;
use crate::events;

pub use attachments::AttachmentBlob;
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
}

impl EventStore {
    /// Open or create the database at `db_path`.
    pub fn open(db_path: &Path, max_events_per_session: usize) -> Result<Self> {
        // Prefix "acp" maps to the existing acp_events / acp_attachments tables.
        let schema = events::Schema::new("acp")?;
        let conn = events::open(db_path, &schema)?;
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
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Append one event (a duplicate seq is ignored), then prune past the retention cap.
    pub fn record(&self, session_id: &str, seq: u64, event: &Event) -> Result<()> {
        let json = serde_json::to_string(event)
            .with_context(|| format!("serialise event for {session_id}@{seq}"))?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let inserted = events::insert_event(&tx, &self.schema, session_id, seq, &json, now_ms)?;
        if inserted != 0 {
            rate_limit::update_rate_limit_budget(&tx, &self.schema, session_id, seq, event);
        }
        tx.commit()?;
        trace!(
            target: "acp.event_store",
            session = %session_id,
            seq,
            kind = event_kind(&json),
            bytes = json.len(),
            duplicate = inserted == 0,
            "recorded event"
        );
        events::prune_retention(
            &conn,
            &self.schema,
            session_id,
            self.max_events_per_session,
            RETENTION_PINNED_EVENT_DISCRIMINANTS,
        );
        if self.max_events_per_session != 0 && seq.is_multiple_of(COMPLETED_WORK_SWEEP_INTERVAL) {
            prune_completed_background_work(
                &conn,
                &self.schema,
                session_id,
                self.max_events_per_session,
            );
        }
        Ok(())
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
        let json = serde_json::to_string(event)?;
        events::insert_event(
            &self.conn(),
            &self.schema,
            session_id,
            seq,
            &json,
            created_at_ms,
        )?;
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

    /// Every session with stored events, with its highest seq.
    pub fn all_session_seqs(&self) -> Vec<(String, u64)> {
        let collected = events::all_topic_seqs(&self.conn(), &self.schema);
        debug!(
            target: "acp.event_store",
            sessions = collected.len(),
            "all_session_seqs hydration"
        );
        collected
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
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for table in [
            self.schema.events_table(),
            self.schema.attachments_table(),
            self.schema.pending_attachments_table(),
            self.schema.rate_limit_budgets_table(),
        ] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE session_id = ?1"),
                rusqlite::params![session_id],
            )?;
        }
        tx.commit()?;
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
        let deleted = events::delete_topic(&self.conn(), &self.schema, session_id);
        debug!(
            target: "acp.event_store",
            session = %session_id,
            deleted,
            "deleted session events"
        );
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
        // An end closes only the starts before it: ids may be reused.
        let delete_started = format!(
            "DELETE FROM {events} AS started
             WHERE started.session_id = ?1 AND started.seq <= ?2
               AND started.discriminant = '{start}'
               AND EXISTS (
                   SELECT 1 FROM {events} AS ended
                   WHERE ended.session_id = ?1 AND ended.discriminant = '{end}'
                     AND ended.seq > started.seq
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
                .record(session_id, first_seq + i as u64, &event)
                .unwrap();
        }
    }

    pub(in crate::acp::event_store) fn seqs(events: &[(u64, Event)]) -> Vec<u64> {
        events.iter().map(|(seq, _)| *seq).collect()
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
        store.record("s-1", 1, &user_prompt("hi")).unwrap();
        store.record("s-1", 1, &Event::ThinkingStarted).unwrap();
        store.record("s-1", 2, &agent_chunk("hi back")).unwrap();
        store.record("s-2", 1, &Event::ThinkingStarted).unwrap();
        drop(store);

        let store = EventStore::open(&tmp.path().join("acp.db"), 1000).unwrap();
        let replay = store.replay_from("s-1", 0);
        assert_eq!(seqs(&replay), [1, 2]);
        assert!(
            matches!(&replay[0].1, Event::UserPromptSent { text, .. } if text == "hi"),
            "the first write at a seq wins"
        );
        let mut listed = store.all_session_seqs();
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

        store.record("shapes", 1, &prompt(Some("repoA"))).unwrap();
        match &store.replay_from("shapes", 0)[0].1 {
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
    fn retention_drops_the_oldest_but_keeps_snapshot_events() {
        let (_tmp, store) = open_store(3);
        record_from(&store, "plain", 1, (1..=5).map(|_| Event::ThinkingStarted));
        assert_eq!(seqs(&store.replay_from("plain", 0)), [3, 4, 5]);

        let snapshots = [
            Event::AvailableCommandsUpdated { commands: vec![] },
            Event::ModesAvailable {
                current_mode_id: "default".into(),
                modes: vec![],
            },
            Event::AcpSessionAssigned {
                acp_session_id: "acp-xyz".into(),
            },
        ];
        record_from(&store, "s-1", 1, snapshots);
        record_from(&store, "s-1", 4, (4..=20).map(|_| Event::ThinkingStarted));
        let kept = seqs(&store.replay_from("s-1", 0));
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
