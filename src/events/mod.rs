//! Protocol-agnostic durable event log: the storage substrate behind the
//! ACP transcript store and, in time, the plugin host's event bus.
//!
//! This module owns the SQLite mechanics that have nothing to do with any
//! particular event payload: schema creation, the append + per-topic
//! retention prune, keyset row scans, seq bookkeeping, attachment blob
//! storage, and topic deletion. Events are opaque JSON strings keyed by an
//! arbitrary `topic` (the partition key), with a caller-assigned monotonic
//! `seq`. The consumer owns its payload type, its replay semantics, and any
//! payload-aware queries; it holds the [`rusqlite::Connection`] and threads it plus a
//! [`crate::events::Schema`] into these free functions. The dependency arrow runs consumer
//! -> here, never the reverse.
//!
//! ## On-disk shape
//!
//! Two tables per [`crate::events::Schema`], named `<prefix>_events` and
//! `<prefix>_attachments`. The partition key is physically the `session_id`
//! column (kept under that name so an existing ACP database loads without a
//! migration) even though the API speaks of "topics". The payload column is
//! `event_json`. A consumer's payload-aware SQL may rely on those column
//! names; they are part of this module's contract.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use tracing::{debug, warn};

/// Names the two tables an [`EventLog`-style consumer](self) reads and
/// writes, derived from a validated prefix. Construct once at open time and
/// thread it into every call so table-name construction is centralized.
#[derive(Debug, Clone)]
pub struct Schema {
    events_table: String,
    attachments_table: String,
}

impl Schema {
    /// Build a schema from `prefix`. The prefix is validated to
    /// `[a-z_]+` so it can be interpolated into table/index names (SQLite
    /// cannot bind identifiers as parameters) without any injection risk.
    pub fn new(prefix: &str) -> Result<Self> {
        if prefix.is_empty() || !prefix.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
            anyhow::bail!("event log table prefix must be non-empty and match [a-z_]+");
        }
        Ok(Self {
            events_table: format!("{prefix}_events"),
            attachments_table: format!("{prefix}_attachments"),
        })
    }

    pub fn events_table(&self) -> &str {
        &self.events_table
    }

    pub fn attachments_table(&self) -> &str {
        &self.attachments_table
    }
}

/// Which side of a seq cursor a [`scan`] window sits on.
#[derive(Debug, Clone, Copy)]
pub enum SeqBound {
    /// Rows with `seq > value` (forward from a replay cursor).
    After(u64),
    /// Rows with `seq < value` (older history below a cursor).
    Before(u64),
}

/// Row ordering for a [`scan`] window.
#[derive(Debug, Clone, Copy)]
pub enum Order {
    Asc,
    Desc,
}

/// Build an `AND event_json NOT LIKE ?` SQL fragment (one per discriminant in
/// `prefixes`) plus the matching bound LIKE patterns, newline-joined for
/// readable queries. Used both to pin events against retention eviction and to
/// exclude them from activity scans; the caller supplies the set, so this
/// module stays payload-agnostic. The discriminant is bound as a parameter
/// rather than interpolated into the SQL text, so a future consumer's prefix
/// containing a quote can't break or inject into the predicate. The clause
/// uses anonymous `?` placeholders, so callers must build their full parameter
/// list positionally (topic / cutoff first, then these patterns in order).
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

/// Open or create the database at `db_path` and ensure `schema`'s tables
/// and indexes exist. WAL mode is enabled so a writer (append path) and a
/// reader (replay) don't block each other.
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
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {events} (
            session_id  TEXT    NOT NULL,
            seq         INTEGER NOT NULL,
            event_json  TEXT    NOT NULL,
            created_at  INTEGER NOT NULL,
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
            ON {attachments}(session_id, seq);"
    ))
    .context("create event log schema")?;
    Ok(conn)
}

/// Append one opaque event payload. Idempotent on duplicate `(topic, seq)`
/// thanks to the primary key; re-appending the same seq is a no-op.
/// Returns the number of rows inserted (0 on a duplicate) so the caller can
/// distinguish a fresh write from a benign retry.
pub fn insert_event(
    conn: &Connection,
    schema: &Schema,
    topic: &str,
    seq: u64,
    json: &str,
    created_at: i64,
) -> Result<usize> {
    let sql = format!(
        "INSERT OR IGNORE INTO {} (session_id, seq, event_json, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        schema.events_table()
    );
    conn.execute(&sql, params![topic, seq as i64, json, created_at])
        .with_context(|| format!("insert {topic}@{seq}"))
}

/// Prune the oldest events for `topic` beyond `max_events`, exempting any
/// event whose payload starts with one of `pinned_prefixes` (matched on the
/// externally-tagged JSON discriminant). Attachment blobs at or below the
/// prune cutoff are dropped in the same pass so they stay bounded alongside
/// the log. No-op when `max_events` is 0. Failures are logged and swallowed:
/// the row is already recorded, we just exceed the cap until the next prune.
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
    // Compute the prune cutoff seq once so the events delete and the
    // attachments delete agree on the same threshold. Doing the events
    // delete first would shift the OFFSET row out from under the
    // attachments delete and leave orphaned blobs.
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
    // Prune the events first. If this fails, return before touching
    // attachments: deleting blobs while their owning events survive would
    // leave replay events pointing at missing data.
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
    // Drop blobs whose owning event was just pruned (no longer present at or
    // below the cutoff). Tying the delete to event existence rather than a
    // flat `seq <= cutoff` keeps a pinned event's blobs, instead of assuming
    // pinned variants never carry attachments, so a future consumer can't
    // strand a surviving event's blob.
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

/// Fetch up to `limit` raw `(seq, json)` rows for `topic` on the given side
/// of the cursor, in the given order. `limit` of `None` is unbounded. The
/// caller owns deserialization, has-more probing (pass `limit + 1`), and
/// cursor advancement; this just runs the keyset query. Rows that fail to
/// decode at the SQLite layer are skipped (effectively never, given the NOT
/// NULL schema).
pub fn scan(
    conn: &Connection,
    schema: &Schema,
    topic: &str,
    bound: SeqBound,
    order: Order,
    limit: Option<usize>,
) -> Vec<(u64, String)> {
    // `seq` is a signed SQLite column; clamp before the cast so a
    // `u64::MAX` cursor (the status probe / tail request) doesn't wrap to a
    // negative bound and match the wrong rows.
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

/// Highest seq stored for `topic`, or 0 if none.
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

/// Lowest seq still stored for `topic`, or `None` when empty.
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

/// Every topic with at least one event, paired with its highest seq, in one
/// query. Used to re-seed per-topic seq counters at startup.
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

/// Most recent `created_at` per topic among `topics`, excluding events
/// whose payload matches one of `excluded_prefixes`. Topics with no
/// qualifying event are absent from the map. Empty `topics` returns empty.
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
    // Positional params: the IN(...) topics first, then the NOT LIKE patterns.
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

/// Persist one attachment blob keyed to `(topic, attachment_id)`, riding
/// with event `seq` so retention and topic deletion drop it in lockstep.
/// Idempotent on `(topic, attachment_id)`. Returns `true` on success.
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

/// Drop all attachment blobs owned by one event seq (a rollback when the
/// owning event could not be durably persisted).
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

/// Fetch one attachment's `(mime_type, data)`, scoped by `topic` so a token
/// for one topic can't read another's blob by guessing ids.
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

/// Drop every event and attachment for `topic`. Returns the number of event
/// rows deleted.
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
    deleted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem(schema: &Schema) -> Connection {
        // An in-memory DB shares the schema-creation path with `open`.
        let conn = Connection::open_in_memory().unwrap();
        let events = schema.events_table();
        let attachments = schema.attachments_table();
        conn.execute_batch(&format!(
            "CREATE TABLE {events} (session_id TEXT NOT NULL, seq INTEGER NOT NULL, event_json TEXT NOT NULL, created_at INTEGER NOT NULL, PRIMARY KEY (session_id, seq));
             CREATE TABLE {attachments} (session_id TEXT NOT NULL, seq INTEGER NOT NULL, attachment_id TEXT NOT NULL, kind TEXT NOT NULL, mime_type TEXT NOT NULL, name TEXT, data BLOB NOT NULL, created_at INTEGER NOT NULL, PRIMARY KEY (session_id, attachment_id));"
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

    /// The log is genuinely topic-keyed and payload-opaque: drive it with a
    /// non-ACP prefix and two topics, proving append/scan/seq/delete all
    /// partition correctly. This is what makes the substrate reusable.
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
        // Duplicate (topic, seq) is a no-op.
        assert_eq!(
            insert_event(&conn, &schema, "a", 2, "\"dup\"", 0).unwrap(),
            0
        );

        assert_eq!(highest_seq(&conn, &schema, "a"), 3);
        assert_eq!(lowest_seq(&conn, &schema, "a"), Some(1));
        assert_eq!(highest_seq(&conn, &schema, "b"), 1);
        assert_eq!(highest_seq(&conn, &schema, "missing"), 0);
        assert_eq!(lowest_seq(&conn, &schema, "missing"), None);

        // Forward scan after a cursor, bounded.
        let fwd = scan(&conn, &schema, "a", SeqBound::After(0), Order::Asc, Some(2));
        assert_eq!(fwd, vec![(1, "\"e1\"".into()), (2, "\"e2\"".into())]);
        // Backward scan below a cursor.
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
    fn retention_prunes_oldest_but_keeps_pinned() {
        let schema = Schema::new("demo").unwrap();
        let conn = mem(&schema);
        // seq 1 is a pinned snapshot; 2..=5 are ordinary.
        insert_event(&conn, &schema, "t", 1, "{\"Pinned\":{}}", 1).unwrap();
        for seq in 2..=5u64 {
            insert_event(&conn, &schema, "t", seq, "{\"Chunk\":{}}", seq as i64).unwrap();
        }
        // Cap of 2 with Pinned exempt: keep newest 2 (4,5) plus pinned 1.
        prune_retention(&conn, &schema, "t", 2, &["Pinned"]);
        let kept: Vec<u64> = scan(&conn, &schema, "t", SeqBound::After(0), Order::Asc, None)
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        assert_eq!(kept, vec![1, 4, 5]);
    }

    /// A pinned event's attachment must survive a prune (the prune ties the
    /// attachment delete to the same predicate as the event delete, rather
    /// than assuming pinned events never carry blobs). A pruned event's
    /// attachment must be dropped.
    #[test]
    fn retention_keeps_pinned_event_attachments() {
        let schema = Schema::new("demo").unwrap();
        let conn = mem(&schema);
        insert_event(&conn, &schema, "t", 1, "{\"Pinned\":{}}", 1).unwrap();
        for seq in 2..=5u64 {
            insert_event(&conn, &schema, "t", seq, "{\"Chunk\":{}}", seq as i64).unwrap();
        }
        // Blob on the pinned event (seq 1) and on a soon-pruned event (seq 2).
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
        // Wrong topic can't read it.
        assert_eq!(load_attachment(&conn, &schema, "other", "att-1"), None);
        delete_attachments_for_seq(&conn, &schema, "t", 7);
        assert_eq!(load_attachment(&conn, &schema, "t", "att-1"), None);
    }

    #[test]
    fn last_event_at_excludes_prefixes() {
        let schema = Schema::new("demo").unwrap();
        let conn = mem(&schema);
        insert_event(&conn, &schema, "t", 1, "{\"Chunk\":{}}", 100).unwrap();
        // A later, but excluded, event must not advance the activity clock.
        insert_event(&conn, &schema, "t", 2, "{\"Snapshot\":{}}", 200).unwrap();
        let map = last_event_at_for_topics(&conn, &schema, &["t".into()], &["Snapshot"]);
        assert_eq!(map.get("t"), Some(&100));
    }
}
