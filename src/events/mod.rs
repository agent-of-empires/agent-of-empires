//! Protocol-agnostic durable event log over SQLite. Payloads are opaque JSON keyed by topic
//! and a caller-assigned `seq`. Tables are `<prefix>_events` and `<prefix>_attachments`; the
//! topic column is named `session_id` and the payload `event_json`, which consumers may rely on.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use tracing::{debug, warn};

#[derive(Debug, Clone)]
pub struct Schema {
    events_table: String,
    attachments_table: String,
    pending_attachments_table: String,
    rate_limit_budgets_table: String,
}

impl Schema {
    /// `[a-z_]+` only: SQLite cannot bind identifiers, so the prefix is interpolated.
    pub fn new(prefix: &str) -> Result<Self> {
        if prefix.is_empty() || !prefix.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
            anyhow::bail!("event log table prefix must be non-empty and match [a-z_]+");
        }
        Ok(Self {
            events_table: format!("{prefix}_events"),
            attachments_table: format!("{prefix}_attachments"),
            pending_attachments_table: format!("{prefix}_pending_attachments"),
            rate_limit_budgets_table: format!("{prefix}_rate_limit_budgets"),
        })
    }

    pub fn events_table(&self) -> &str {
        &self.events_table
    }

    pub fn attachments_table(&self) -> &str {
        &self.attachments_table
    }

    /// Keyed by an opaque `ref_id` and outside the retention prune, so blobs survive until
    /// their owning action fires.
    pub fn pending_attachments_table(&self) -> &str {
        &self.pending_attachments_table
    }

    /// Outside the retention prune so the redelivery cap survives every history cap.
    pub fn rate_limit_budgets_table(&self) -> &str {
        &self.rate_limit_budgets_table
    }
}

#[derive(Debug, Clone, Copy)]
pub enum SeqBound {
    After(u64),
    Before(u64),
}

#[derive(Debug, Clone, Copy)]
pub enum Order {
    Asc,
    Desc,
}

/// Uses anonymous `?` placeholders, so callers bind positionally: topic or cutoff first,
/// then these patterns in order.
fn not_like_clauses(prefixes: &[&str]) -> (String, Vec<String>) {
    let fragment = prefixes
        .iter()
        .map(|_| "AND event_json NOT LIKE ?")
        .collect::<Vec<_>>()
        .join("\n               ");
    let patterns = prefixes
        .iter()
        .map(|name| format!("{{\"{name}\":%"))
        .collect();
    (fragment, patterns)
}

pub fn open(db_path: &Path, schema: &Schema) -> Result<Connection> {
    if let Some(parent) = db_path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("create parent dir for event log at {}", parent.display())
            })?;
        }
    }
    let conn = Connection::open(db_path)
        .with_context(|| format!("open event log at {}", db_path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .context("enable WAL mode")?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .context("set synchronous=NORMAL")?;
    let events = schema.events_table();
    let attachments = schema.attachments_table();
    let pending_attachments = schema.pending_attachments_table();
    let rate_limit_budgets = schema.rate_limit_budgets_table();
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {events} (
            session_id   TEXT    NOT NULL,
            seq          INTEGER NOT NULL,
            event_json   TEXT    NOT NULL,
            created_at   INTEGER NOT NULL,
            discriminant TEXT,
            PRIMARY KEY (session_id, seq)
        );
        CREATE INDEX IF NOT EXISTS idx_{events}_session_seq
            ON {events}(session_id, seq);
        CREATE INDEX IF NOT EXISTS idx_{events}_session_created_at
            ON {events}(session_id, created_at);
        CREATE TABLE IF NOT EXISTS {attachments} (
            session_id    TEXT    NOT NULL,
            seq           INTEGER NOT NULL,
            attachment_id TEXT    NOT NULL,
            kind          TEXT    NOT NULL,
            mime_type     TEXT    NOT NULL,
            name          TEXT,
            data          BLOB    NOT NULL,
            created_at    INTEGER NOT NULL,
            PRIMARY KEY (session_id, attachment_id)
        );
        CREATE INDEX IF NOT EXISTS idx_{attachments}_session_seq
            ON {attachments}(session_id, seq);
        CREATE TABLE IF NOT EXISTS {pending_attachments} (
            session_id    TEXT    NOT NULL,
            ref_id        TEXT    NOT NULL,
            attachment_id TEXT    NOT NULL,
            kind          TEXT    NOT NULL,
            mime_type     TEXT    NOT NULL,
            name          TEXT,
            data          BLOB    NOT NULL,
            created_at    INTEGER NOT NULL,
            PRIMARY KEY (session_id, attachment_id)
        );
        CREATE INDEX IF NOT EXISTS idx_{pending_attachments}_session_ref
            ON {pending_attachments}(session_id, ref_id);
        CREATE INDEX IF NOT EXISTS idx_{pending_attachments}_created_at
            ON {pending_attachments}(created_at);
        CREATE TABLE IF NOT EXISTS {rate_limit_budgets} (
            session_id    TEXT    NOT NULL PRIMARY KEY,
            spent         INTEGER NOT NULL,
            armed         INTEGER NOT NULL
        );"
    ))
    .context("create event log schema")?;
    ensure_discriminant_column(&conn, events)?;
    Ok(conn)
}

fn ensure_discriminant_column(conn: &Connection, events: &str) -> Result<()> {
    let has_column: bool = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM pragma_table_info('{events}') WHERE name = 'discriminant'"
            ),
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|count| count > 0)
        .unwrap_or(false);
    if !has_column {
        conn.execute_batch(&format!(
            "BEGIN;
             ALTER TABLE {events} ADD COLUMN discriminant TEXT;
             UPDATE {events}
                SET discriminant = substr(
                    event_json,
                    instr(event_json, '\"') + 1,
                    instr(substr(event_json, instr(event_json, '\"') + 1), '\"') - 1
                )
              WHERE discriminant IS NULL AND instr(event_json, '\"') > 0;
             COMMIT;"
        ))
        .context("backfill discriminant column")?;
    }
    conn.execute_batch(&format!(
        "CREATE INDEX IF NOT EXISTS idx_{events}_session_discriminant_seq
            ON {events}(session_id, discriminant, seq);"
    ))
    .context("create discriminant index")?;
    Ok(())
}

/// The first quoted token, which is the variant name for both data and unit variants.
fn discriminant_of(json: &str) -> &str {
    let Some(open) = json.find('"') else {
        return "";
    };
    let rest = &json[open + 1..];
    match rest.find('"') {
        Some(close) => &rest[..close],
        None => "",
    }
}

pub fn latest_by_discriminant(
    conn: &Connection,
    schema: &Schema,
    topic: &str,
    discriminant: &str,
) -> Option<(u64, String)> {
    conn.query_row(
        &format!(
            "SELECT seq, event_json FROM {}
             WHERE session_id = ?1 AND discriminant = ?2
             ORDER BY seq DESC LIMIT 1",
            schema.events_table()
        ),
        params![topic, discriminant],
        |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, String>(1)?)),
    )
    .optional()
    .ok()
    .flatten()
}

pub fn insert_event(
    conn: &Connection,
    schema: &Schema,
    topic: &str,
    seq: u64,
    json: &str,
    created_at: i64,
) -> Result<usize> {
    let sql = format!(
        "INSERT OR IGNORE INTO {} (session_id, seq, event_json, created_at, discriminant)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        schema.events_table()
    );
    conn.execute(
        &sql,
        params![topic, seq as i64, json, created_at, discriminant_of(json)],
    )
    .with_context(|| format!("insert {topic}@{seq}"))
}

pub fn count_since(
    conn: &Connection,
    schema: &Schema,
    topic: &str,
    min_created_at: i64,
) -> Result<u64> {
    let sql = format!(
        "SELECT COUNT(*) FROM {} WHERE session_id = ?1 AND created_at >= ?2",
        schema.events_table()
    );
    let count: i64 = conn
        .query_row(&sql, params![topic, min_created_at], |row| row.get(0))
        .with_context(|| format!("count events for {topic}"))?;
    Ok(count.max(0) as u64)
}

pub fn prune_retention(
    conn: &Connection,
    schema: &Schema,
    topic: &str,
    max_events: usize,
    pinned_prefixes: &[&str],
) {
    if max_events == 0 {
        return;
    }
    // Compute the cutoff once so the events and attachments deletes agree.
    let events = schema.events_table();
    let attachments = schema.attachments_table();
    let cutoff: Option<i64> = conn
        .query_row(
            &format!(
                "SELECT seq FROM {events}
                 WHERE session_id = ?1
                 ORDER BY seq DESC
                 LIMIT 1 OFFSET ?2"
            ),
            params![topic, max_events as i64],
            |row| row.get(0),
        )
        .optional()
        .unwrap_or(None);
    let Some(cutoff) = cutoff else {
        return;
    };
    let (clauses, patterns) = not_like_clauses(pinned_prefixes);
    // Deleting blobs whose events survive would leave replay pointing at missing data.
    let prune_sql = format!("DELETE FROM {events} WHERE session_id = ? AND seq <= ? {clauses}");
    let mut prune_params: Vec<Value> = vec![Value::Text(topic.to_owned()), Value::Integer(cutoff)];
    prune_params.extend(patterns.into_iter().map(Value::Text));
    match conn.execute(&prune_sql, params_from_iter(prune_params)) {
        Ok(0) => return,
        Ok(pruned) => {
            debug!(
                target: "events",
                topic = %topic,
                pruned,
                cap = max_events,
                "pruned oldest events past retention cap"
            );
        }
        Err(e) => {
            warn!(target: "events", "prune {topic}: {e}");
            return;
        }
    }
    // Tie the delete to event existence so a pinned event keeps its blobs.
    if let Err(e) = conn.execute(
        &format!(
            "DELETE FROM {attachments}
             WHERE session_id = ?1
               AND seq <= ?2
               AND seq NOT IN (SELECT seq FROM {events} WHERE session_id = ?1)"
        ),
        params![topic, cutoff],
    ) {
        warn!(target: "events", "prune attachments {topic}: {e}");
    }
}

pub fn scan(
    conn: &Connection,
    schema: &Schema,
    topic: &str,
    bound: SeqBound,
    order: Order,
    limit: Option<usize>,
) -> Vec<(u64, String)> {
    // Clamp before the cast so a `u64::MAX` cursor does not wrap negative.
    let (op, value) = match bound {
        SeqBound::After(v) => (">", v),
        SeqBound::Before(v) => ("<", v),
    };
    let value_i64 = i64::try_from(value).unwrap_or(i64::MAX);
    let order_sql = match order {
        Order::Asc => "ASC",
        Order::Desc => "DESC",
    };
    let events = schema.events_table();
    let sql = match limit {
        Some(_) => format!(
            "SELECT seq, event_json FROM {events}
             WHERE session_id = ?1 AND seq {op} ?2
             ORDER BY seq {order_sql} LIMIT ?3"
        ),
        None => format!(
            "SELECT seq, event_json FROM {events}
             WHERE session_id = ?1 AND seq {op} ?2
             ORDER BY seq {order_sql}"
        ),
    };
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            warn!(target: "events", "prepare scan for {topic}: {e}");
            return Vec::new();
        }
    };
    let map_row = |row: &rusqlite::Row| {
        let seq: i64 = row.get(0)?;
        let json: String = row.get(1)?;
        Ok((seq as u64, json))
    };
    let rows = match limit {
        Some(n) => stmt.query_map(params![topic, value_i64, n as i64], map_row),
        None => stmt.query_map(params![topic, value_i64], map_row),
    };
    let rows = match rows {
        Ok(r) => r,
        Err(e) => {
            warn!(target: "events", "query scan for {topic}: {e}");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for row in rows {
        match row {
            Ok(pair) => out.push(pair),
            Err(e) => warn!(target: "events", "row error: {e}"),
        }
    }
    out
}

pub fn highest_seq(conn: &Connection, schema: &Schema, topic: &str) -> u64 {
    match conn
        .query_row(
            &format!(
                "SELECT MAX(seq) FROM {} WHERE session_id = ?1",
                schema.events_table()
            ),
            params![topic],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()
    {
        Ok(Some(Some(max))) => max as u64,
        _ => 0,
    }
}

pub fn lowest_seq(conn: &Connection, schema: &Schema, topic: &str) -> Option<u64> {
    match conn
        .query_row(
            &format!(
                "SELECT MIN(seq) FROM {} WHERE session_id = ?1",
                schema.events_table()
            ),
            params![topic],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()
    {
        Ok(Some(Some(m))) => Some(m as u64),
        _ => None,
    }
}

pub fn all_topic_seqs(conn: &Connection, schema: &Schema) -> Vec<(String, u64)> {
    let sql = format!(
        "SELECT session_id, MAX(seq) FROM {} GROUP BY session_id",
        schema.events_table()
    );
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            warn!(target: "events", "prepare all_topic_seqs: {e}");
            return Vec::new();
        }
    };
    let rows = match stmt.query_map([], |row| {
        let id: String = row.get(0)?;
        let max: i64 = row.get(1)?;
        Ok((id, max as u64))
    }) {
        Ok(r) => r,
        Err(e) => {
            warn!(target: "events", "query all_topic_seqs: {e}");
            return Vec::new();
        }
    };
    rows.filter_map(|r| r.ok()).collect()
}

pub fn last_event_at_for_topics(
    conn: &Connection,
    schema: &Schema,
    topics: &[String],
    excluded_prefixes: &[&str],
) -> HashMap<String, i64> {
    let mut out = HashMap::new();
    if topics.is_empty() {
        return out;
    }
    let placeholders = std::iter::repeat_n("?", topics.len())
        .collect::<Vec<_>>()
        .join(",");
    let (clauses, patterns) = not_like_clauses(excluded_prefixes);
    let sql = format!(
        "SELECT session_id, MAX(created_at) FROM {events}
         WHERE session_id IN ({placeholders})
           {clauses}
         GROUP BY session_id",
        events = schema.events_table(),
    );
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            warn!(target: "events", "last_event_at_for_topics prepare: {e}");
            return out;
        }
    };
    let mut bind: Vec<Value> = topics.iter().map(|t| Value::Text(t.clone())).collect();
    bind.extend(patterns.into_iter().map(Value::Text));
    let rows = stmt.query_map(params_from_iter(bind), |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    });
    match rows {
        Ok(iter) => {
            for r in iter {
                match r {
                    Ok((topic, created_at)) => {
                        out.insert(topic, created_at);
                    }
                    Err(e) => warn!(target: "events", "last_event_at_for_topics row: {e}"),
                }
            }
        }
        Err(e) => warn!(target: "events", "last_event_at_for_topics query: {e}"),
    }
    out
}

#[allow(clippy::too_many_arguments)]
pub fn insert_attachment(
    conn: &Connection,
    schema: &Schema,
    topic: &str,
    seq: u64,
    attachment_id: &str,
    kind: &str,
    mime_type: &str,
    name: Option<&str>,
    data: &[u8],
    created_at: i64,
) -> bool {
    let sql = format!(
        "INSERT OR IGNORE INTO {}
            (session_id, seq, attachment_id, kind, mime_type, name, data, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        schema.attachments_table()
    );
    if let Err(e) = conn.execute(
        &sql,
        params![
            topic,
            seq as i64,
            attachment_id,
            kind,
            mime_type,
            name,
            data,
            created_at
        ],
    ) {
        warn!(
            target: "events",
            topic = %topic,
            attachment = %attachment_id,
            "insert attachment failed: {e}"
        );
        return false;
    }
    true
}

pub fn delete_attachments_for_seq(conn: &Connection, schema: &Schema, topic: &str, seq: u64) {
    if let Err(e) = conn.execute(
        &format!(
            "DELETE FROM {} WHERE session_id = ?1 AND seq = ?2",
            schema.attachments_table()
        ),
        params![topic, seq as i64],
    ) {
        warn!(
            target: "events",
            topic = %topic,
            seq,
            "rollback attachments failed: {e}"
        );
    }
}

pub fn load_attachment(
    conn: &Connection,
    schema: &Schema,
    topic: &str,
    attachment_id: &str,
) -> Option<(String, Vec<u8>)> {
    conn.query_row(
        &format!(
            "SELECT mime_type, data FROM {} WHERE session_id = ?1 AND attachment_id = ?2",
            schema.attachments_table()
        ),
        params![topic, attachment_id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
    )
    .optional()
    .unwrap_or_else(|e| {
        warn!(
            target: "events",
            topic = %topic,
            attachment = %attachment_id,
            "load attachment failed: {e}"
        );
        None
    })
}

#[allow(clippy::too_many_arguments)]
pub fn insert_pending_attachment(
    conn: &Connection,
    schema: &Schema,
    topic: &str,
    ref_id: &str,
    attachment_id: &str,
    kind: &str,
    mime_type: &str,
    name: Option<&str>,
    data: &[u8],
    created_at: i64,
) -> bool {
    let sql = format!(
        "INSERT OR IGNORE INTO {}
            (session_id, ref_id, attachment_id, kind, mime_type, name, data, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        schema.pending_attachments_table()
    );
    if let Err(e) = conn.execute(
        &sql,
        params![
            topic,
            ref_id,
            attachment_id,
            kind,
            mime_type,
            name,
            data,
            created_at
        ],
    ) {
        warn!(
            target: "events",
            topic = %topic,
            attachment = %attachment_id,
            "insert pending attachment failed: {e}"
        );
        return false;
    }
    true
}

#[allow(clippy::type_complexity)]
pub fn load_pending_attachments_for_ref(
    conn: &Connection,
    schema: &Schema,
    topic: &str,
    ref_id: &str,
) -> Vec<(String, String, String, Option<String>, Vec<u8>)> {
    let sql = format!(
        "SELECT attachment_id, kind, mime_type, name, data FROM {}
         WHERE session_id = ?1 AND ref_id = ?2
         ORDER BY rowid",
        schema.pending_attachments_table()
    );
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            warn!(target: "events", topic = %topic, "prepare load pending attachments: {e}");
            return Vec::new();
        }
    };
    let rows = stmt.query_map(params![topic, ref_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Vec<u8>>(4)?,
        ))
    });
    match rows {
        Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
        Err(e) => {
            warn!(target: "events", topic = %topic, "load pending attachments: {e}");
            Vec::new()
        }
    }
}

pub fn delete_pending_attachments_for_ref(
    conn: &Connection,
    schema: &Schema,
    topic: &str,
    ref_id: &str,
) {
    if let Err(e) = conn.execute(
        &format!(
            "DELETE FROM {} WHERE session_id = ?1 AND ref_id = ?2",
            schema.pending_attachments_table()
        ),
        params![topic, ref_id],
    ) {
        warn!(
            target: "events",
            topic = %topic,
            ref_id = %ref_id,
            "delete pending attachments failed: {e}"
        );
    }
}

pub fn pending_attachment_bytes_for_session(
    conn: &Connection,
    schema: &Schema,
    topic: &str,
) -> u64 {
    conn.query_row(
        &format!(
            "SELECT COALESCE(SUM(LENGTH(data)), 0) FROM {} WHERE session_id = ?1",
            schema.pending_attachments_table()
        ),
        params![topic],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n.max(0) as u64)
    .unwrap_or(0)
}

pub fn prune_pending_attachments_older_than(
    conn: &Connection,
    schema: &Schema,
    cutoff_ms: i64,
) -> usize {
    match conn.execute(
        &format!(
            "DELETE FROM {} WHERE created_at <= ?1",
            schema.pending_attachments_table()
        ),
        params![cutoff_ms],
    ) {
        Ok(n) => n,
        Err(e) => {
            warn!(target: "events", "prune pending attachments: {e}");
            0
        }
    }
}

pub fn delete_topic(conn: &Connection, schema: &Schema, topic: &str) -> usize {
    let deleted = match conn.execute(
        &format!(
            "DELETE FROM {} WHERE session_id = ?1",
            schema.events_table()
        ),
        params![topic],
    ) {
        Ok(n) => n,
        Err(e) => {
            warn!(target: "events", "delete {topic}: {e}");
            0
        }
    };
    if let Err(e) = conn.execute(
        &format!(
            "DELETE FROM {} WHERE session_id = ?1",
            schema.attachments_table()
        ),
        params![topic],
    ) {
        warn!(target: "events", "delete attachments {topic}: {e}");
    }
    if let Err(e) = conn.execute(
        &format!(
            "DELETE FROM {} WHERE session_id = ?1",
            schema.pending_attachments_table()
        ),
        params![topic],
    ) {
        warn!(target: "events", "delete pending attachments {topic}: {e}");
    }
    if let Err(e) = conn.execute(
        &format!(
            "DELETE FROM {} WHERE session_id = ?1",
            schema.rate_limit_budgets_table()
        ),
        params![topic],
    ) {
        warn!(target: "events", "delete rate-limit budget {topic}: {e}");
    }
    deleted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem(schema: &Schema) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        let events = schema.events_table();
        let attachments = schema.attachments_table();
        let pending = schema.pending_attachments_table();
        conn.execute_batch(&format!(
            "CREATE TABLE {events} (session_id TEXT NOT NULL, seq INTEGER NOT NULL, event_json TEXT NOT NULL, created_at INTEGER NOT NULL, discriminant TEXT, PRIMARY KEY (session_id, seq));
             CREATE INDEX idx_{events}_session_discriminant_seq ON {events}(session_id, discriminant, seq);
             CREATE TABLE {attachments} (session_id TEXT NOT NULL, seq INTEGER NOT NULL, attachment_id TEXT NOT NULL, kind TEXT NOT NULL, mime_type TEXT NOT NULL, name TEXT, data BLOB NOT NULL, created_at INTEGER NOT NULL, PRIMARY KEY (session_id, attachment_id));
             CREATE TABLE {pending} (session_id TEXT NOT NULL, ref_id TEXT NOT NULL, attachment_id TEXT NOT NULL, kind TEXT NOT NULL, mime_type TEXT NOT NULL, name TEXT, data BLOB NOT NULL, created_at INTEGER NOT NULL, PRIMARY KEY (session_id, attachment_id));"
        ))
        .unwrap();
        conn
    }

    #[test]
    fn schema_rejects_bad_prefix() {
        assert!(Schema::new("").is_err());
        assert!(Schema::new("ACP").is_err());
        assert!(Schema::new("plugin-host").is_err());
        assert!(Schema::new("plugin1").is_err());
        let s = Schema::new("plugin_host").unwrap();
        assert_eq!(s.events_table(), "plugin_host_events");
        assert_eq!(s.attachments_table(), "plugin_host_attachments");
    }

    #[test]
    fn topic_keyed_append_scan_and_delete() {
        let schema = Schema::new("demo").unwrap();
        let conn = mem(&schema);
        for (topic, seq) in [("a", 1u64), ("a", 2), ("a", 3), ("b", 1)] {
            assert_eq!(
                insert_event(
                    &conn,
                    &schema,
                    topic,
                    seq,
                    &format!("\"e{seq}\""),
                    seq as i64
                )
                .unwrap(),
                1
            );
        }
        assert_eq!(
            insert_event(&conn, &schema, "a", 2, "\"dup\"", 0).unwrap(),
            0
        );

        assert_eq!(highest_seq(&conn, &schema, "a"), 3);
        assert_eq!(lowest_seq(&conn, &schema, "a"), Some(1));
        assert_eq!(highest_seq(&conn, &schema, "b"), 1);
        assert_eq!(highest_seq(&conn, &schema, "missing"), 0);
        assert_eq!(lowest_seq(&conn, &schema, "missing"), None);

        let fwd = scan(&conn, &schema, "a", SeqBound::After(0), Order::Asc, Some(2));
        assert_eq!(fwd, vec![(1, "\"e1\"".into()), (2, "\"e2\"".into())]);
        let back = scan(
            &conn,
            &schema,
            "a",
            SeqBound::Before(u64::MAX),
            Order::Desc,
            Some(2),
        );
        assert_eq!(back, vec![(3, "\"e3\"".into()), (2, "\"e2\"".into())]);

        let mut seqs = all_topic_seqs(&conn, &schema);
        seqs.sort();
        assert_eq!(seqs, vec![("a".into(), 3), ("b".into(), 1)]);

        assert_eq!(delete_topic(&conn, &schema, "a"), 3);
        assert_eq!(highest_seq(&conn, &schema, "a"), 0);
        assert_eq!(highest_seq(&conn, &schema, "b"), 1);
    }

    #[test]
    fn retention_prunes_oldest_but_keeps_pinned_events_and_their_attachments() {
        let schema = Schema::new("demo").unwrap();
        let conn = mem(&schema);
        insert_event(&conn, &schema, "t", 1, "{\"Pinned\":{}}", 1).unwrap();
        for seq in 2..=5u64 {
            insert_event(&conn, &schema, "t", seq, "{\"Chunk\":{}}", seq as i64).unwrap();
        }
        insert_attachment(
            &conn,
            &schema,
            "t",
            1,
            "pinned-att",
            "image",
            "image/png",
            None,
            b"keep",
            0,
        );
        insert_attachment(
            &conn,
            &schema,
            "t",
            2,
            "pruned-att",
            "image",
            "image/png",
            None,
            b"drop",
            0,
        );
        prune_retention(&conn, &schema, "t", 2, &["Pinned"]);
        let kept: Vec<u64> = scan(&conn, &schema, "t", SeqBound::After(0), Order::Asc, None)
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        assert_eq!(kept, vec![1, 4, 5]);
        assert!(
            load_attachment(&conn, &schema, "t", "pinned-att").is_some(),
            "blob owned by a pinned (surviving) event must be kept"
        );
        assert!(
            load_attachment(&conn, &schema, "t", "pruned-att").is_none(),
            "blob owned by a pruned event must be dropped"
        );
    }

    #[test]
    fn attachments_roundtrip_and_scope() {
        let schema = Schema::new("demo").unwrap();
        let conn = mem(&schema);
        assert!(insert_attachment(
            &conn,
            &schema,
            "t",
            7,
            "att-1",
            "image",
            "image/png",
            Some("shot.png"),
            b"bytes",
            0
        ));
        let got = load_attachment(&conn, &schema, "t", "att-1");
        assert_eq!(got, Some(("image/png".into(), b"bytes".to_vec())));
        assert_eq!(load_attachment(&conn, &schema, "other", "att-1"), None);
        delete_attachments_for_seq(&conn, &schema, "t", 7);
        assert_eq!(load_attachment(&conn, &schema, "t", "att-1"), None);
    }

    #[test]
    fn latest_by_discriminant_returns_newest_match() {
        let schema = Schema::new("demo").unwrap();
        let conn = mem(&schema);
        insert_event(&conn, &schema, "t", 1, "{\"PlanUpdated\":{\"n\":1}}", 1).unwrap();
        insert_event(&conn, &schema, "t", 2, "{\"Chunk\":{}}", 2).unwrap();
        insert_event(&conn, &schema, "t", 3, "{\"PlanUpdated\":{\"n\":2}}", 3).unwrap();
        insert_event(&conn, &schema, "t", 4, "{\"Chunk\":{}}", 4).unwrap();
        assert_eq!(
            latest_by_discriminant(&conn, &schema, "t", "PlanUpdated"),
            Some((3, "{\"PlanUpdated\":{\"n\":2}}".to_string()))
        );
        assert_eq!(
            latest_by_discriminant(&conn, &schema, "t", "WakeupScheduled"),
            None
        );
        let last_at = last_event_at_for_topics(&conn, &schema, &["t".into()], &["Chunk"]);
        assert_eq!(last_at.get("t"), Some(&3), "excluded prefixes are skipped");
        insert_event(&conn, &schema, "t", 5, "\"ThinkingStarted\"", 5).unwrap();
        assert_eq!(
            latest_by_discriminant(&conn, &schema, "t", "ThinkingStarted"),
            Some((5, "\"ThinkingStarted\"".to_string()))
        );
        insert_event(&conn, &schema, "other", 9, "{\"PlanUpdated\":{\"n\":9}}", 9).unwrap();
        assert_eq!(
            latest_by_discriminant(&conn, &schema, "t", "PlanUpdated"),
            Some((3, "{\"PlanUpdated\":{\"n\":2}}".to_string()))
        );

        let plan: String = conn
            .query_row(
                &format!(
                    "EXPLAIN QUERY PLAN
                     SELECT seq, event_json FROM {}
                     WHERE session_id = ?1 AND discriminant = ?2
                     ORDER BY seq DESC LIMIT 1",
                    schema.events_table()
                ),
                params!["t", "PlanUpdated"],
                |row| row.get(3),
            )
            .unwrap();
        assert!(
            plan.contains("idx_demo_events_session_discriminant_seq"),
            "expected the discriminant index to be used, got plan: {plan}"
        );
    }

    #[test]
    fn ensure_discriminant_column_backfills_legacy_rows() {
        let schema = Schema::new("demo").unwrap();
        let events = schema.events_table();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE {events} (session_id TEXT NOT NULL, seq INTEGER NOT NULL, event_json TEXT NOT NULL, created_at INTEGER NOT NULL, PRIMARY KEY (session_id, seq));"
        ))
        .unwrap();
        for (seq, json) in [
            (1i64, "{\"PlanUpdated\":{\"n\":1}}"),
            (2, "{\"Chunk\":{}}"),
            (3, "\"ThinkingStarted\""),
        ] {
            conn.execute(
                &format!(
                    "INSERT INTO {events} (session_id, seq, event_json, created_at) VALUES ('t', ?1, ?2, ?1)"
                ),
                params![seq, json],
            )
            .unwrap();
        }
        ensure_discriminant_column(&conn, events).unwrap();
        assert_eq!(
            latest_by_discriminant(&conn, &schema, "t", "PlanUpdated"),
            Some((1, "{\"PlanUpdated\":{\"n\":1}}".to_string()))
        );
        assert_eq!(
            latest_by_discriminant(&conn, &schema, "t", "ThinkingStarted"),
            Some((3, "\"ThinkingStarted\"".to_string()))
        );
        ensure_discriminant_column(&conn, events).unwrap();
        assert_eq!(
            latest_by_discriminant(&conn, &schema, "t", "Chunk"),
            Some((2, "{\"Chunk\":{}}".to_string()))
        );
    }

    #[test]
    fn pending_attachments_store_roundtrip_survives_retention_prune() {
        let schema = Schema::new("demo").unwrap();
        let conn = mem(&schema);
        let put = |ref_id: &str, att: &str, data: &[u8], at: i64| {
            assert!(insert_pending_attachment(
                &conn,
                &schema,
                "t",
                ref_id,
                att,
                "image",
                "image/png",
                Some("a.png"),
                data,
                at,
            ));
        };
        put("q1", "a1", b"one", 100);
        put("q1", "a2", b"twotwo", 100);
        put("q2", "b1", b"three", 100);

        let q1 = load_pending_attachments_for_ref(&conn, &schema, "t", "q1");
        assert_eq!(
            q1.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
            ["a1", "a2"]
        );
        assert_eq!(q1[0].4, b"one");

        assert_eq!(
            pending_attachment_bytes_for_session(&conn, &schema, "t"),
            14
        );

        put("q1", "a1", b"ignored", 100);
        assert_eq!(
            pending_attachment_bytes_for_session(&conn, &schema, "t"),
            14
        );

        prune_retention(&conn, &schema, "t", 0, &[]);
        assert_eq!(
            pending_attachment_bytes_for_session(&conn, &schema, "t"),
            14
        );

        delete_pending_attachments_for_ref(&conn, &schema, "t", "q1");
        assert!(load_pending_attachments_for_ref(&conn, &schema, "t", "q1").is_empty());
        assert_eq!(pending_attachment_bytes_for_session(&conn, &schema, "t"), 5);

        assert!(insert_pending_attachment(
            &conn,
            &schema,
            "t",
            "q3",
            "c1",
            "image",
            "image/png",
            None,
            b"new",
            500,
        ));
        assert_eq!(prune_pending_attachments_older_than(&conn, &schema, 100), 1);
        assert_eq!(pending_attachment_bytes_for_session(&conn, &schema, "t"), 3);

        delete_topic(&conn, &schema, "t");
        assert_eq!(pending_attachment_bytes_for_session(&conn, &schema, "t"), 0);
    }
}
