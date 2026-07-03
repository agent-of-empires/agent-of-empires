//! Disk-backed event log for structured view sessions.
//!
//! Every event published through `ChannelSink` is appended here so the
//! conversation transcript survives page reloads, session switches,
//! and `aoe serve` restarts. One row per `(session_id, seq)` with a
//! per-session retention cap; older events are pruned on insert once
//! the row count exceeds the cap.
//!
//! ## How replay flows
//!
//! - **WebSocket on-connect drain.** The client passes the `lastSeq` it
//!   has cached (or 0 on first connect) as a query param to
//!   `/sessions/{id}/acp/ws`. The handler reads
//!   `replay_from(session_id, since)` out of this store and pushes
//!   those frames before forwarding the live broadcast, closing the
//!   subscribe-gap race that would otherwise drop the agent's first
//!   chunks on a fast page load.
//! - **Snapshot endpoint.** `GET /acp/replay?since=N` reads the
//!   same data path, used by the React reducer when it sees a `lagged`
//!   notice from the WS to catch up missed frames.
//! - **Startup hydration.** On boot, `next_seqs` is rehydrated from
//!   `MAX(seq) + 1` per session so post-restart writes don't collide
//!   with pre-restart rows via `INSERT OR IGNORE`.
//!
//! ## How it relates to agent-side memory
//!
//! This store only persists the *UI transcript*. The model's
//! conversation context across `aoe serve` restarts is a separate
//! mechanism in `supervisor.rs`: when the agent advertises
//! `agent_capabilities.load_session = true` on the ACP `initialize`
//! response, the supervisor stores the agent-assigned `session_id` on
//! `Instance.acp_session_id` and uses `session/load` on
//! subsequent spawns instead of `session/new`. If `session/load`
//! fails, the stored id is cleared and a `SessionContextReset` event
//! is published; the UI renders an amber callout in the transcript so
//! the user knows prior turns are no longer in the model's context.
//!
//! The bundled `aoe-agent` does not yet advertise `load_session`, so
//! its UI transcript replays from this store on restart but the model
//! itself starts fresh each spawn (tracked in #1005).
//!
//! ## Lifecycle
//!
//! Per-session rows are dropped on session delete and on
//! `acp_disable` (a per-session switch back to the terminal view). The
//! connection has WAL mode enabled so the publish path
//! and the replay endpoint don't block each other under load.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use tracing::{debug, trace, warn};

use super::approvals::Nonce;
use super::state::{Event, Plan, RateLimitInfo};
use crate::events::{self, Order, SeqBound};

/// Externally-tagged JSON discriminants for non-substantive structured view
/// events: lifecycle and metadata snapshots the agent emits once per
/// session (or on every cold-start resume) that must not count as session
/// activity. Two SQL predicates depend on this set staying in sync: the
/// retention prune exempts them from eviction (#1049) and the idle-reap
/// idle clock ignores them (#1689). Centralized so the two cannot silently
/// desync.
/// A launched / progressing background agent stops counting toward
/// `has_in_flight_turn` after this long with no fresh progress and no
/// terminal event. A live tailer emits a progress snapshot every ~1.5s, so
/// a gap this large means the tailer died (e.g. a daemon crash) and no
/// `BackgroundAgentCompleted` will ever arrive; without this bound such an
/// agent would pin the build-stale respawn pass forever. Comfortably past
/// the tailer's 300s `ABORT_AFTER`. See #2573.
const BACKGROUND_AGENT_STALE_AFTER_MS: i64 = 6 * 60 * 1000;

const NON_SUBSTANTIVE_EVENT_DISCRIMINANTS: &[&str] = &[
    "AvailableCommandsUpdated",
    "ModesAvailable",
    "CurrentModeChanged",
    "AcpSessionAssigned",
    "PromptCapabilities",
];

/// One page of replayed events plus the cursor metadata a paginating
/// client needs to fetch the next page.
pub struct ReplayPage {
    /// Deserialised events for this page, oldest first. Rows that fail
    /// to deserialise are skipped here but still advance `last_scanned_seq`
    /// so a corrupt row can never stall a paging loop.
    pub events: Vec<(u64, Event)>,
    /// Highest `seq` this page consumed, whether or not it deserialised.
    /// The client passes this back as `since` for the next page, so the
    /// cursor advances past skipped/corrupt rows. `None` for an empty page.
    pub last_scanned_seq: Option<u64>,
    /// True when at least one row exists beyond this page's window.
    /// Derived from a `LIMIT n + 1` probe row, so it is consistent with
    /// the page rows under the same lock and never depends on a
    /// separately queried `highest_seq`.
    pub has_more: bool,
    /// Highest seq stored for the session, or 0 if none. Read under the
    /// same lock as the page rows so the replay response is a single
    /// consistent snapshot (a concurrent `record()` can't make `has_more`
    /// and `highest_seq` disagree). See #1705 review.
    pub highest_seq: u64,
    /// Lowest seq still stored, or `None` when empty. Same-snapshot
    /// guarantee as `highest_seq`; lets the caller compute `lost`.
    pub lowest_seq: Option<u64>,
}

/// A user turn begins at one of these events. Backward replay paging
/// (`replay_page_before`) aligns each page to one so the client never
/// seams a split turn when it prepends older history.
fn is_user_turn_boundary(ev: &Event) -> bool {
    matches!(
        ev,
        Event::UserPromptSent { .. } | Event::UserDiffCommentsPrompt { .. }
    )
}

/// SQLite-backed structured view event log. One row per (session_id, seq).
///
/// The generic storage mechanics (schema, append, retention prune, keyset
/// scans, seq bookkeeping, attachment blobs, topic deletion) live in
/// `crate::events`; this type is the ACP consumer: it owns the connection,
/// serializes/deserializes the `Event` payload, and keeps the ACP-specific
/// replay semantics and `json_extract` accessors. The dependency arrow runs
/// acp -> events.
pub struct EventStore {
    conn: Mutex<Connection>,
    /// Read-only connection used exclusively by `search_content`. A
    /// content search scans many rows; routing it through the writer
    /// `conn` would hold that mutex for the whole scan and stall the
    /// live `record()` path (WAL gives no concurrency while one Rust
    /// mutex serializes every access). A separate read-only connection
    /// lets WAL serve the scan concurrently with writes. Searches still
    /// serialize against each other behind this mutex, which is fine:
    /// one in-flight search at a time is plenty for the palette.
    search_conn: Mutex<Connection>,
    schema: events::Schema,
    /// Per-session retention cap. Older events are pruned on insert
    /// once the count exceeds this value. Bytes are not enforced here
    /// (the in-memory ring still has a byte cap); the row count keeps
    /// the on-disk size bounded.
    max_events_per_session: usize,
}

impl EventStore {
    /// Open or create the database at `db_path`. Creates the
    /// `acp_events` table if missing. The connection has WAL mode
    /// enabled so concurrent writers (publish path) and readers
    /// (replay endpoint) don't block each other.
    pub fn open(db_path: &Path, max_events_per_session: usize) -> Result<Self> {
        // Prefix "acp" maps to the existing acp_events / acp_attachments
        // tables, so an established database opens unchanged (no migration).
        let schema = events::Schema::new("acp")?;
        let conn = events::open(db_path, &schema)?;
        // Separate read-only handle for content search; the writer above
        // already created the file and tables, so opening read-only here
        // always succeeds. query_only is belt-and-suspenders on top of the
        // READ_ONLY flag; busy_timeout keeps a scan from erroring out if it
        // briefly contends with a checkpoint.
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

    /// Append one event. Idempotent on duplicate (session_id, seq) thanks
    /// to the primary key; re-publishing the same seq is a no-op.
    /// Returns Err when the event was *not* persisted, so the caller can
    /// surface the gap (e.g. publish a `Lagged` frame on the broadcast
    /// channel) instead of letting the on-disk log silently fall behind
    /// the in-memory broadcast subscribers.
    pub fn record(&self, session_id: &str, seq: u64, event: &Event) -> Result<()> {
        let json = serde_json::to_string(event)
            .with_context(|| format!("serialise event for {session_id}@{seq}"))?;
        let bytes = json.len();
        let kind = event_kind(event);
        let now_ms = chrono::Utc::now().timestamp_millis();
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let inserted = events::insert_event(&conn, &self.schema, session_id, seq, &json, now_ms)?;
        if inserted == 0 {
            // Primary-key collision: same (session_id, seq) seen before.
            // Logged at trace because the cause is usually a benign retry
            // (publish_user_prompt + replay drain re-publishing) rather
            // than a bug, but we still want a breadcrumb. Per-event lines
            // are too noisy to live at debug; they bury the lifecycle
            // signal in debug.log during an active turn.
            trace!(
                target: "acp.event_store",
                session = %session_id,
                seq,
                kind,
                "skipped duplicate event (already on disk)"
            );
        } else {
            trace!(
                target: "acp.event_store",
                session = %session_id,
                seq,
                kind,
                bytes,
                "recorded event"
            );
        }
        // Prune oldest beyond the retention cap on every insert so the
        // per-session disk bound stays strict rather than amortised.
        // NON_SUBSTANTIVE_EVENT_DISCRIMINANTS are exempt: the agent emits
        // those snapshot events (slash-command list, mode list, ACP session
        // id) once per session lifecycle near the start of the seq range, so
        // a long session would otherwise evict them and leave the composer's
        // `/` palette and the mode picker empty on reconnect. See #1049.
        events::prune_retention(
            &conn,
            &self.schema,
            session_id,
            self.max_events_per_session,
            NON_SUBSTANTIVE_EVENT_DISCRIMINANTS,
        );
        Ok(())
    }

    /// Test-only: record an event with an explicit `created_at` (ms epoch)
    /// so recency-sensitive probes (e.g. the background-agent staleness bound
    /// in `has_in_flight_turn`) can be exercised deterministically.
    #[cfg(test)]
    pub(crate) fn record_at(
        &self,
        session_id: &str,
        seq: u64,
        event: &Event,
        created_at_ms: i64,
    ) -> Result<()> {
        let json = serde_json::to_string(event)?;
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        events::insert_event(&conn, &self.schema, session_id, seq, &json, created_at_ms)?;
        Ok(())
    }

    /// Return all events for `session_id` with `seq < before`, oldest
    /// first. Used by the context-primer endpoint to fetch only the
    /// transcript that precedes a `SessionContextReset` event without
    /// having to over-fetch and filter client-side. See #1004.
    pub fn replay_before(&self, session_id: &str, before: u64) -> Vec<(u64, Event)> {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        events::scan(
            &conn,
            &self.schema,
            session_id,
            SeqBound::Before(before),
            Order::Asc,
            None,
        )
        .into_iter()
        .filter_map(|(seq, json)| match serde_json::from_str::<Event>(&json) {
            Ok(event) => Some((seq, event)),
            Err(e) => {
                warn!(
                    target: "acp.event_store",
                    "deserialise event {session_id}@{seq}: {e}"
                );
                None
            }
        })
        .collect()
    }

    /// Return all events for `session_id` with `seq > since`, oldest
    /// first. An empty vec means the session has no newer events.
    ///
    /// Unbounded; used by the WS on-connect drain and tests. The REST
    /// replay endpoint pages via [`replay_page`](Self::replay_page).
    pub fn replay_from(&self, session_id: &str, since: u64) -> Vec<(u64, Event)> {
        self.replay_page(session_id, since, None).events
    }

    /// Return events for `session_id` with `seq > since`, oldest first,
    /// at most `limit` of them. `None` means unbounded (no `LIMIT`).
    ///
    /// Pagination is keyset over `seq`: callers pass the previous page's
    /// `last_scanned_seq` back as `since`. When `limit` is `Some(n)` the
    /// query probes `n + 1` rows so `has_more` reflects whether another
    /// page exists, computed under the same lock as the page rows (so it
    /// never races a concurrently growing `highest_seq`). `last_scanned_seq`
    /// advances over every consumed row including ones that fail to
    /// deserialise, so a corrupt row can't trap a paging loop.
    pub fn replay_page(&self, session_id: &str, since: u64, limit: Option<usize>) -> ReplayPage {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        // Snapshot the bounds under the same lock as the page rows so the
        // whole response is consistent: a concurrent `record()` cannot
        // land between these reads and the page query and make
        // `has_more`/`highest_seq` disagree (which would let the client's
        // paging cap stop early). See #1705 review.
        let highest_seq = events::highest_seq(&conn, &self.schema, session_id);
        let lowest_seq = events::lowest_seq(&conn, &self.schema, session_id);
        // Probe one extra row beyond `limit` to detect `has_more` without a
        // second query against `highest_seq`. The signed-column clamp for a
        // `u64::MAX` cursor lives in `events::scan`.
        let probe = limit.map(|n| n.saturating_add(1));
        let rows = events::scan(
            &conn,
            &self.schema,
            session_id,
            SeqBound::After(since),
            Order::Asc,
            probe,
        );
        let mut out = Vec::new();
        let mut last_scanned_seq = None;
        let mut has_more = false;
        for (scanned, (seq, json)) in rows.into_iter().enumerate() {
            // The probe row proves another page exists; don't consume it or
            // advance the cursor onto it.
            if let Some(n) = limit {
                if scanned == n {
                    has_more = true;
                    break;
                }
            }
            last_scanned_seq = Some(seq);
            match serde_json::from_str::<Event>(&json) {
                Ok(event) => out.push((seq, event)),
                Err(e) => warn!(
                    target: "acp.event_store",
                    "deserialise event {session_id}@{seq}: {e}"
                ),
            }
        }
        trace!(
            target: "acp.event_store",
            session = %session_id,
            since,
            limit = ?limit,
            returned = out.len(),
            has_more,
            "replayed events"
        );
        ReplayPage {
            events: out,
            last_scanned_seq,
            has_more,
            highest_seq,
            lowest_seq,
        }
    }

    /// Return up to `limit` events for `session_id` with `seq < before`,
    /// the ones sitting CLOSEST below `before`, in ascending (oldest
    /// first) order. Backs the structured view's recent-first load: the
    /// client renders the tail first (request `before = u64::MAX`) and
    /// pages older history as the user scrolls up, passing the previous
    /// page's lowest seq back as `before`.
    ///
    /// Mirrors [`replay_page`](Self::replay_page)'s single-snapshot lock
    /// and `n + 1` probe, but scans `ORDER BY seq DESC` so the window is
    /// the newest rows below `before` (a forward `ORDER BY seq ASC LIMIT`
    /// would return the OLDEST rows and leave a gap). The DESC result is
    /// reversed in memory before returning so callers always see ASC.
    /// `last_scanned_seq` is the LOWEST seq consumed (the cursor for the
    /// next-older page); it advances over rows that fail to deserialise so
    /// a corrupt row can't stall the paging loop.
    pub fn replay_page_before(
        &self,
        session_id: &str,
        before: u64,
        limit: Option<usize>,
    ) -> ReplayPage {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let highest_seq = events::highest_seq(&conn, &self.schema, session_id);
        let lowest_seq = events::lowest_seq(&conn, &self.schema, session_id);
        // Probe one extra row beyond `limit` to detect `has_more`. The
        // signed-column clamp for a `u64::MAX` tail cursor lives in
        // `events::scan`.
        let probe = limit.map(|n| n.saturating_add(1));
        let rows = events::scan(
            &conn,
            &self.schema,
            session_id,
            SeqBound::Before(before),
            Order::Desc,
            probe,
        );
        let mut out = Vec::new();
        let mut last_scanned_seq = None;
        let mut has_more = false;
        for (scanned, (seq, json)) in rows.into_iter().enumerate() {
            // The probe row proves an older page exists; don't consume it or
            // move the cursor onto it.
            if let Some(n) = limit {
                if scanned == n {
                    has_more = true;
                    break;
                }
            }
            // DESC scan, so each consumed row's seq is lower than the last;
            // the final one is the page's lowest, which is the cursor for
            // the next-older page.
            last_scanned_seq = Some(seq);
            match serde_json::from_str::<Event>(&json) {
                Ok(event) => out.push((seq, event)),
                Err(e) => warn!(
                    target: "acp.event_store",
                    "deserialise event {session_id}@{seq}: {e}"
                ),
            }
        }
        // Scanned newest-first; callers expect oldest-first.
        out.reverse();
        // Align the page to a user-turn boundary so the client can reduce
        // it in isolation and prepend it without seaming a split turn onto
        // the already-loaded head. Only when `has_more`: a leading partial
        // turn belongs to an older turn whose start is in the next page, so
        // drop it here and let the next `before` re-fetch it. When the page
        // reached session start (`!has_more`) keep everything, so the very
        // first turn and the pinned handshake snapshot (#1049) survive on a
        // short session that loads in one page. The giant-single-turn case
        // (no boundary in the whole window) keeps the window as-is rather
        // than returning an empty page and trapping the client's paging loop.
        if has_more {
            if let Some(i) = out
                .iter()
                .position(|(_, ev)| is_user_turn_boundary(ev))
                .filter(|&i| i > 0)
            {
                out.drain(0..i);
            }
        }
        // Cursor for the next-older page is the lowest seq we kept, so a
        // trimmed leading turn re-loads contiguously on the next request.
        last_scanned_seq = out.first().map(|(seq, _)| *seq).or(last_scanned_seq);
        trace!(
            target: "acp.event_store",
            session = %session_id,
            before,
            limit = ?limit,
            returned = out.len(),
            has_more,
            "replayed events before cursor"
        );
        ReplayPage {
            events: out,
            last_scanned_seq,
            has_more,
            highest_seq,
            lowest_seq,
        }
    }

    /// Return the latest `Event::PlanUpdated` stored for `session_id`,
    /// if any. Used by the REST sessions endpoint to surface
    /// plan-progress chrome (current step / completed / total) on the
    /// sidebar without subscribing to the structured view WS for every session.
    /// See #1061.
    pub fn latest_plan(&self, session_id: &str) -> Option<Plan> {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let (_, json) =
            events::latest_by_discriminant(&conn, &self.schema, session_id, "PlanUpdated")?;
        let event: Event = serde_json::from_str(&json).ok()?;
        if let Event::PlanUpdated { plan } = event {
            Some(plan)
        } else {
            None
        }
    }

    /// Full-text search over conversation content (agent messages, user
    /// prompts, tool output) across every session, newest match first,
    /// grouped to one hit per session. Backs the web command palette's
    /// "Conversations" group (#2515).
    ///
    /// Runs on the dedicated read-only `search_conn` so a scan never
    /// blocks the live `record()` path. Returns an empty vec for a query
    /// below the minimum length or on any DB error: search is best-effort
    /// chrome, not a correctness path.
    ///
    // ponytail: raw event_json LIKE prefilter, then decode + verify in
    // Rust. No FTS5 and no plaintext sidecar table, so zero write-path
    // cost and no migration. The SQL LIKE can over-match on JSON keys
    // (e.g. "text"), but the post-decode check that the extracted prose
    // actually contains the query drops those before they reach results.
    // Ceiling: a full scan bounded by SEARCH_ROW_SCAN_CAP. Upgrade path
    // if the corpus outgrows it: a plaintext sidecar table or an FTS5
    // virtual table populated on record().
    pub fn search_content(&self, query: &str, limit: usize) -> Vec<ContentHit> {
        let trimmed = query.trim();
        if trimmed.chars().count() < MIN_SEARCH_CHARS {
            return Vec::new();
        }
        let needle: String = trimmed
            .chars()
            .take(MAX_SEARCH_CHARS)
            .collect::<String>()
            .to_lowercase();
        let limit = limit.clamp(1, MAX_SEARCH_RESULTS);
        let pattern = format!("%{}%", escape_like(&needle));

        let conn = match self.search_conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let mut stmt = match conn.prepare(
            "SELECT session_id, seq, event_json FROM acp_events
             WHERE event_json LIKE ?1 ESCAPE '\\'
             ORDER BY created_at DESC LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(e) => {
                warn!(target: "acp.event_store", "prepare search query: {e}");
                return Vec::new();
            }
        };
        let rows = match stmt.query_map(params![pattern, SEARCH_ROW_SCAN_CAP as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        }) {
            Ok(r) => r,
            Err(e) => {
                warn!(target: "acp.event_store", "run search query: {e}");
                return Vec::new();
            }
        };

        // Preserve newest-first row order; the first row seen for a
        // session is its representative (newest) hit, later rows only
        // bump match_count.
        let mut order: Vec<String> = Vec::new();
        let mut hits: std::collections::HashMap<String, ContentHit> =
            std::collections::HashMap::new();
        for row in rows.flatten() {
            let (session_id, seq, json) = row;
            let Ok(event) = serde_json::from_str::<Event>(&json) else {
                continue;
            };
            let Some((kind, text)) = event_search_text(&event) else {
                continue;
            };
            // The SQL LIKE matched the raw JSON; confirm the query is in
            // the extracted prose, not a field name or escape sequence.
            if !text.to_lowercase().contains(&needle) {
                continue;
            }
            match hits.get_mut(&session_id) {
                Some(hit) => hit.match_count += 1,
                None => {
                    // Stop once we have enough distinct sessions: rows are
                    // newest-first, so any further new session ranks below
                    // these and would be dropped anyway. Counting the cap on
                    // distinct sessions (not raw rows) keeps one chatty
                    // session from hiding others. match_count past this point
                    // is left approximate on purpose.
                    if order.len() >= limit {
                        break;
                    }
                    order.push(session_id.clone());
                    hits.insert(
                        session_id.clone(),
                        ContentHit {
                            session_id,
                            seq: seq.max(0) as u64,
                            kind,
                            snippet: make_snippet(&text, &needle),
                            match_count: 1,
                        },
                    );
                }
            }
        }

        order
            .into_iter()
            .filter_map(|id| hits.remove(&id))
            .collect()
    }

    /// Return the most recent `Event::RateLimit` stored for `session_id`
    /// together with the wall-clock millis it was recorded (the
    /// `created_at` row column). The reconciler's rate-limit auto-resume
    /// pass reads this to decide whether `resets_at + grace` has elapsed,
    /// using `created_at` as the floor for a minimum park window so a
    /// buggy adapter reporting a past `resets_at` cannot trigger a tight
    /// respawn loop. Latest event wins (a re-rate-limit supersedes the
    /// prior reset time). See #1722.
    pub fn latest_rate_limit_event(&self, session_id: &str) -> Option<(RateLimitInfo, i64)> {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let row: Option<(String, i64)> = conn
            .query_row(
                "SELECT event_json, created_at FROM acp_events
                 WHERE session_id = ?1
                   AND discriminant = 'RateLimit'
                 ORDER BY seq DESC LIMIT 1",
                params![session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .ok()
            .flatten();
        let (json, created_at) = row?;
        let event: Event = serde_json::from_str(&json).ok()?;
        if let Event::RateLimit { info } = event {
            Some((info, created_at))
        } else {
            None
        }
    }

    /// Return the most recent unfired `WakeupScheduled` for `session_id`.
    /// "Pending" means the latest scheduled `at` is still in the future;
    /// the previous heuristic (any `UserPromptSent` with a higher seq
    /// marks the wakeup as fired) is wrong because a user-typed
    /// follow-up message during the wait wasn't the wake firing; the
    /// next ScheduleWakeup turn could still arrive minutes later. Pick
    /// the latest WakeupScheduled and gate on the timestamp instead.
    /// See #1091.
    pub fn latest_pending_wakeup(
        &self,
        session_id: &str,
    ) -> Option<(DateTime<Utc>, Option<String>)> {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let json: Option<String> =
            events::latest_by_discriminant(&conn, &self.schema, session_id, "WakeupScheduled")
                .map(|(_, json)| json);
        // No log for the "no row" branch. The web UI polls /api/sessions
        // every ~2-3s and fans this query out per structured view session; every
        // idle session would land here on every poll. The past-due branch
        // below is silent for the same reason: once a wakeup's `at` is in
        // the past it stays past forever, so logging there would spam one
        // line per poll for the session's whole life. Only the bounded
        // "still pending" branch logs; it carries the wake `at` and clears
        // the moment the wakeup fires.
        let json = json?;
        let event: Event = match serde_json::from_str(&json) {
            Ok(e) => e,
            Err(e) => {
                warn!(
                    target: "acp.event_store",
                    session = %session_id,
                    "latest_pending_wakeup: deserialise failed: {e}"
                );
                return None;
            }
        };
        if let Event::WakeupScheduled { at, reason } = event {
            let now = Utc::now();
            if at > now {
                trace!(
                    target: "acp.event_store",
                    session = %session_id,
                    wake_at = %at,
                    in_secs = (at - now).num_seconds(),
                    "latest_pending_wakeup: still pending"
                );
                Some((at, reason))
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Whether the session has an armed `Monitor` (background watch). Returns
    /// the latest `MonitorArmed` description when active, `None` otherwise.
    ///
    /// A `Monitor` is fire-and-forget with no fixed end time, so unlike
    /// `latest_pending_wakeup` there is no timestamp to gate on, and the
    /// `Monitor` tool call itself completes at arm time (it returns "monitor
    /// started"), so its completion is not the disarm signal either. The
    /// badge stays up from the arm until either the user takes over
    /// (`UserPromptSent` at a higher seq) or the monitor fires and that turn
    /// ends. The fire is observed as a tool call started after the arm (the
    /// agent acting on the wake); the badge then retires on the next
    /// `Stopped`. This covers both shapes: the agent ending the arming turn
    /// and resuming later between prompts (closed by `agent_idle`), and the
    /// monitor blocking the arming turn in-band (closed by `prompt_complete`).
    /// A `Stopped` with no post-arm tool work is the arming turn ending while
    /// the monitor is still pending, so it leaves the badge up. See #2325.
    pub fn latest_active_monitor(&self, session_id: &str) -> Option<Option<String>> {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        // Latest MonitorArmed and its seq.
        let (armed_seq, json) =
            events::latest_by_discriminant(&conn, &self.schema, session_id, "MonitorArmed")?;
        let armed_seq = armed_seq as i64;
        // The user taking over clears the badge immediately. No log on the
        // common "no monitor" branch above (this query fans out per
        // structured session on every ~2-3s sessions poll).
        let user_took_over: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM acp_events
                 WHERE session_id = ?1
                   AND seq > ?2
                   AND discriminant = 'UserPromptSent'
                 LIMIT 1",
                params![session_id, armed_seq],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten();
        if user_took_over.is_some() {
            return None;
        }
        // Otherwise the badge clears once the monitor fired (a tool call
        // started after the arm) AND that turn has since ended (a Stopped
        // past that tool start). Without the post-work gate, the arming
        // turn's own Stopped while the monitor is still pending would clear
        // it prematurely.
        let first_work_seq: Option<i64> = conn
            .query_row(
                "SELECT MIN(seq) FROM acp_events
                 WHERE session_id = ?1
                   AND seq > ?2
                   AND discriminant = 'ToolCallStarted'",
                params![session_id, armed_seq],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten();
        if let Some(work_seq) = first_work_seq {
            let turn_ended: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM acp_events
                     WHERE session_id = ?1
                       AND seq > ?2
                       AND discriminant = 'Stopped'
                     LIMIT 1",
                    params![session_id, work_seq],
                    |row| row.get(0),
                )
                .optional()
                .ok()
                .flatten();
            if turn_ended.is_some() {
                return None;
            }
        }
        match serde_json::from_str::<Event>(&json) {
            Ok(Event::MonitorArmed { description }) => Some(description),
            Ok(_) => None,
            Err(e) => {
                warn!(
                    target: "acp.event_store",
                    session = %session_id,
                    "latest_active_monitor: deserialise failed: {e}"
                );
                None
            }
        }
    }

    /// Given the seq of a just-published `UserPromptSent`, return the
    /// `WakeupScheduled` whose timer just fired (so the structured view event
    /// listener can dispatch a push notification). A prompt counts as
    /// the wake-fired prompt when:
    ///
    /// 1. There is a `WakeupScheduled` with seq < `prompt_seq` for this
    ///    session.
    /// 2. The wakeup's `at` timestamp is at-or-before the prompt's
    ///    `created_at` (the scheduled moment has actually elapsed by
    ///    the time the prompt arrived; a user-typed message *during*
    ///    the wait must not count as the wake firing).
    /// 3. No earlier prompt has already "claimed" the same wakeup,
    ///    i.e. no `UserPromptSent` exists with seq strictly between the
    ///    wakeup's seq and `prompt_seq` whose `created_at` is also
    ///    at-or-after the wakeup's `at`. The first prompt past the
    ///    wake's `at` line wins; later prompts are regular follow-ups.
    ///
    /// Returns `None` for the common case (regular user-typed prompt
    /// with no pending wake). See #1091.
    pub fn fired_wakeup_for_prompt(
        &self,
        session_id: &str,
        prompt_seq: u64,
    ) -> Option<(DateTime<Utc>, Option<String>)> {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let prompt_seq_i64 = prompt_seq as i64;
        // Fetch the prompt's own created_at (ms since epoch).
        let prompt_created_ms: i64 = match conn
            .query_row(
                "SELECT created_at FROM acp_events
                 WHERE session_id = ?1 AND seq = ?2",
                params![session_id, prompt_seq_i64],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten()
        {
            Some(v) => v,
            None => {
                trace!(
                    target: "acp.event_store",
                    session = %session_id,
                    seq = prompt_seq,
                    "fired_wakeup_for_prompt: prompt row missing"
                );
                return None;
            }
        };
        // Latest WakeupScheduled with seq < prompt_seq.
        let row: Option<(i64, String)> = conn
            .query_row(
                "SELECT seq, event_json FROM acp_events
                 WHERE session_id = ?1
                   AND seq < ?2
                   AND discriminant = 'WakeupScheduled'
                 ORDER BY seq DESC LIMIT 1",
                params![session_id, prompt_seq_i64],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .ok()
            .flatten();
        let (wake_seq, wake_json) = match row {
            Some(t) => t,
            None => {
                trace!(
                    target: "acp.event_store",
                    session = %session_id,
                    prompt_seq,
                    "fired_wakeup_for_prompt: no prior WakeupScheduled"
                );
                return None;
            }
        };
        let event: Event = match serde_json::from_str(&wake_json) {
            Ok(e) => e,
            Err(e) => {
                warn!(
                    target: "acp.event_store",
                    session = %session_id,
                    wake_seq,
                    "fired_wakeup_for_prompt: deserialise failed: {e}"
                );
                return None;
            }
        };
        let (at, reason) = match event {
            Event::WakeupScheduled { at, reason } => (at, reason),
            _ => return None,
        };
        let at_ms = at.timestamp_millis();
        // Wake must already have fired by the time the prompt arrived.
        if at_ms > prompt_created_ms {
            debug!(
                target: "acp.event_store",
                session = %session_id,
                prompt_seq,
                wake_seq,
                wake_at = %at,
                "fired_wakeup_for_prompt: wake `at` still in future relative to prompt; mid-wait follow-up, not a fire"
            );
            return None;
        }
        // Dedup: another prompt with seq between (wake_seq, prompt_seq)
        // and created_at >= at means *that* prompt already claimed the
        // wake-fire (we'd have fired a push for it then).
        let claimed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM acp_events
                 WHERE session_id = ?1
                   AND seq > ?2
                   AND seq < ?3
                   AND discriminant = 'UserPromptSent'
                   AND created_at >= ?4",
                params![session_id, wake_seq, prompt_seq_i64, at_ms],
                |row| row.get(0),
            )
            .unwrap_or(0);
        if claimed > 0 {
            debug!(
                target: "acp.event_store",
                session = %session_id,
                prompt_seq,
                wake_seq,
                claimed,
                "fired_wakeup_for_prompt: another prompt already claimed this wake"
            );
            return None;
        }
        debug!(
            target: "acp.event_store",
            session = %session_id,
            prompt_seq,
            wake_seq,
            wake_at = %at,
            "fired_wakeup_for_prompt: detected wake-fire"
        );
        Some((at, reason))
    }

    /// Return the highest seq stored for `session_id`, or 0 if none.
    /// Used at startup to re-seed the in-memory `next_seqs` counter so
    /// fresh publishes don't collide with restored history.
    pub fn highest_seq(&self, session_id: &str) -> u64 {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let max = events::highest_seq(&conn, &self.schema, session_id);
        trace!(
            target: "acp.event_store",
            session = %session_id,
            highest_seq = max,
            "highest_seq query"
        );
        max
    }

    /// Return the lowest seq still stored for `session_id`, or `None`
    /// if the session has no events on disk (either never wrote any, or
    /// the retention cap has evicted them all). Used by `/acp/replay`
    /// to compute whether a client's `since` cursor falls below the
    /// pruned floor so the response can signal `lost = true`.
    pub fn lowest_seq(&self, session_id: &str) -> Option<u64> {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let min = events::lowest_seq(&conn, &self.schema, session_id);
        trace!(
            target: "acp.event_store",
            session = %session_id,
            lowest_seq = ?min,
            "lowest_seq query"
        );
        min
    }

    /// Return every session_id that has at least one event stored, with
    /// its highest seq. Used at startup to pre-seed `next_seqs` in one
    /// query rather than racing per-session lookups.
    pub fn all_session_seqs(&self) -> Vec<(String, u64)> {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let collected = events::all_topic_seqs(&conn, &self.schema);
        debug!(
            target: "acp.event_store",
            sessions = collected.len(),
            "all_session_seqs hydration"
        );
        collected
    }

    /// Latest terminal-lifecycle event for `session_id`, used by the
    /// rate-limit park callers to detect a `Stopped{rate_limited}` and
    /// decide whether to auto-resume or hold the session parked.
    ///
    /// `RateLimitAutoResumed` is included deliberately: the rate-limit
    /// auto-resume reconciler pass publishes it to supersede the terminal
    /// `Stopped{rate_limited}`, so this query stops reporting the park and
    /// the main resume loop falls through to a fresh spawn instead of
    /// re-parking. A new status event added here also participates in
    /// park supersession; keep that in mind before extending the set. See
    /// #1722.
    ///
    /// This set intentionally EXCLUDES agent-transcript activity events
    /// (`ThinkingStarted` / `AgentMessageChunk` / `ToolCallStarted`) so an
    /// activity event that lands after a `Stopped{rate_limited}` cannot hide
    /// the park from the resume logic. Status seeding wants the opposite and
    /// uses `latest_seed_status_event` instead. See #2625.
    pub fn latest_status_event(&self, session_id: &str) -> Option<Event> {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let json: Option<String> = conn
            .query_row(
                "SELECT event_json FROM acp_events
                 WHERE session_id = ?1
                   AND (json_extract(event_json, '$.UserPromptSent') IS NOT NULL
                     OR json_extract(event_json, '$.ApprovalRequested') IS NOT NULL
                     OR json_extract(event_json, '$.ApprovalResolved') IS NOT NULL
                     OR json_extract(event_json, '$.ElicitationRequested') IS NOT NULL
                     OR json_extract(event_json, '$.ElicitationResolved') IS NOT NULL
                     OR json_extract(event_json, '$.Stopped') IS NOT NULL
                     OR json_extract(event_json, '$.RateLimitAutoResumed') IS NOT NULL
                     OR json_extract(event_json, '$.AgentStartupError') IS NOT NULL)
                 ORDER BY seq DESC
                 LIMIT 1",
                params![session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .unwrap_or_else(|e| {
                warn!(
                    target: "acp.event_store",
                    "latest_status_event query for {session_id}: {e}"
                );
                None
            });
        json.and_then(|s| serde_json::from_str(&s).ok())
    }

    /// Latest event for `session_id` that the sidebar status derivation
    /// cares about. Used at daemon startup (`seed_acp_statuses`) and on
    /// reattach to seed `Instance.status` from history: the in-memory status
    /// writes that fire on live structured view events don't survive restart,
    /// so without this scan a session that was mid-turn when the previous
    /// daemon died would render Idle until the next lifecycle event arrived.
    /// See #1103.
    ///
    /// Unlike [`Self::latest_status_event`] this set ALSO matches the
    /// agent-transcript activity events (`ThinkingStarted` /
    /// `AgentMessageChunk` / `ToolCallStarted`). The live status path
    /// (`derive_acp_status`) maps those to `Running`, so a turn that the
    /// agent resumed on its own after a `Stopped{prompt_complete}` (no new
    /// `UserPromptSent`, e.g. a fired wakeup or a background job) keeps its
    /// green dot across a restart instead of collapsing to a stale Idle from
    /// the earlier `Stopped`. This query must stay in sync with the
    /// `Set(Running)` / `Set(Waiting)` arms of `derive_acp_status`. See #2625.
    pub fn latest_seed_status_event(&self, session_id: &str) -> Option<Event> {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let json: Option<String> = conn
            .query_row(
                "SELECT event_json FROM acp_events
                 WHERE session_id = ?1
                   AND (json_extract(event_json, '$.UserPromptSent') IS NOT NULL
                     OR json_extract(event_json, '$.ApprovalRequested') IS NOT NULL
                     OR json_extract(event_json, '$.ApprovalResolved') IS NOT NULL
                     OR json_extract(event_json, '$.ElicitationRequested') IS NOT NULL
                     OR json_extract(event_json, '$.ElicitationResolved') IS NOT NULL
                     OR json_extract(event_json, '$.Stopped') IS NOT NULL
                     OR json_extract(event_json, '$.RateLimitAutoResumed') IS NOT NULL
                     OR json_extract(event_json, '$.AgentStartupError') IS NOT NULL
                     OR json_extract(event_json, '$.AgentMessageChunk') IS NOT NULL
                     OR json_extract(event_json, '$.ToolCallStarted') IS NOT NULL
                     -- ThinkingStarted is a unit enum variant, serialized as
                     -- the bare JSON string \"ThinkingStarted\" rather than an
                     -- object, so it needs an equality match, not json_extract.
                     OR event_json = '\"ThinkingStarted\"')
                 ORDER BY seq DESC
                 LIMIT 1",
                params![session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .unwrap_or_else(|e| {
                warn!(
                    target: "acp.event_store",
                    "latest_seed_status_event query for {session_id}: {e}"
                );
                None
            });
        json.and_then(|s| serde_json::from_str(&s).ok())
    }

    /// Text of the session's first `UserPromptSent` event, if any. The manual
    /// "Auto-name now" recovery re-runs smart rename against the original
    /// intent (the first prompt), so it reads the earliest prompt here rather
    /// than depending on the in-memory prompt that the original auto-trigger
    /// saw. Returns `None` when the session has no prompt event (e.g. the first
    /// one was pruned on a very long session), which the caller treats as
    /// "nothing to rename from".
    pub fn first_user_prompt(&self, session_id: &str) -> Option<String> {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let json: Option<String> = conn
            .query_row(
                "SELECT event_json FROM acp_events
                 WHERE session_id = ?1
                   AND json_extract(event_json, '$.UserPromptSent') IS NOT NULL
                 ORDER BY seq ASC
                 LIMIT 1",
                params![session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .unwrap_or_else(|e| {
                warn!(
                    target: "acp.event_store",
                    "first_user_prompt query for {session_id}: {e}"
                );
                None
            });
        match json.and_then(|s| serde_json::from_str::<Event>(&s).ok()) {
            Some(Event::UserPromptSent { text, .. }) => Some(text),
            _ => None,
        }
    }

    /// Nonces of `ApprovalRequested` events for the session that lack a
    /// later `ApprovalResolved` with the same nonce. Used on reattach
    /// to surface "this approval card is dead, the previous daemon's
    /// responder oneshot died with it" so the supervisor can publish a
    /// synthetic `ApprovalResolved { decision: Cancelled }` and the UI
    /// clears the now-404 card. See #1099.
    pub fn unresolved_approval_nonces(&self, session_id: &str) -> Vec<Nonce> {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let mut stmt = match conn.prepare(
            "SELECT json_extract(event_json, '$.ApprovalRequested.approval.nonce') AS nonce
             FROM acp_events
             WHERE session_id = ?1
               AND json_extract(event_json, '$.ApprovalRequested') IS NOT NULL
               AND json_extract(event_json, '$.ApprovalRequested.approval.nonce') NOT IN (
                   SELECT json_extract(event_json, '$.ApprovalResolved.nonce')
                   FROM acp_events
                   WHERE session_id = ?1
                     AND json_extract(event_json, '$.ApprovalResolved') IS NOT NULL
               )",
        ) {
            Ok(s) => s,
            Err(e) => {
                warn!(target: "acp.event_store", "prepare unresolved_approval_nonces for {session_id}: {e}");
                return Vec::new();
            }
        };
        let rows = match stmt.query_map(params![session_id], |row| {
            let nonce: String = row.get(0)?;
            Ok(Nonce(nonce))
        }) {
            Ok(r) => r,
            Err(e) => {
                warn!(target: "acp.event_store", "query unresolved_approval_nonces for {session_id}: {e}");
                return Vec::new();
            }
        };
        rows.filter_map(|r| r.ok()).collect()
    }

    /// Elicitation nonces from `ElicitationRequested` events on disk with
    /// no matching `ElicitationResolved`. The elicitation parallel of
    /// [`Self::unresolved_approval_nonces`]: lets the supervisor cancel
    /// question cards whose responder oneshot died with the previous
    /// daemon, so they don't reappear as dead 404 cards on replay.
    pub fn unresolved_elicitation_nonces(&self, session_id: &str) -> Vec<Nonce> {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let mut stmt = match conn.prepare(
            "SELECT json_extract(event_json, '$.ElicitationRequested.elicitation.nonce') AS nonce
             FROM acp_events
             WHERE session_id = ?1
               AND json_extract(event_json, '$.ElicitationRequested') IS NOT NULL
               AND json_extract(event_json, '$.ElicitationRequested.elicitation.nonce') NOT IN (
                   SELECT json_extract(event_json, '$.ElicitationResolved.nonce')
                   FROM acp_events
                   WHERE session_id = ?1
                     AND json_extract(event_json, '$.ElicitationResolved') IS NOT NULL
               )",
        ) {
            Ok(s) => s,
            Err(e) => {
                warn!(target: "acp.event_store", "prepare unresolved_elicitation_nonces for {session_id}: {e}");
                return Vec::new();
            }
        };
        let rows = match stmt.query_map(params![session_id], |row| {
            let nonce: String = row.get(0)?;
            Ok(Nonce(nonce))
        }) {
            Ok(r) => r,
            Err(e) => {
                warn!(target: "acp.event_store", "query unresolved_elicitation_nonces for {session_id}: {e}");
                return Vec::new();
            }
        };
        rows.filter_map(|r| r.ok()).collect()
    }

    /// True iff the session has a `UserPromptSent` whose turn never
    /// terminated (no later `Stopped` or `AgentStartupError`). Used at
    /// daemon startup to decide whether to synthesize a `Stopped` event
    /// for a session that was mid-turn when the previous `aoe serve`
    /// died, and on reattach to arm the resume-idle watchdog.
    ///
    /// `Stopped` and `AgentStartupError` are serialized externally-tagged
    /// (`{"Stopped":{"reason":"..."}}`) so we match on the variant key
    /// via `json_extract($.Stopped)`.
    pub fn has_in_flight_turn(&self, session_id: &str) -> bool {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let prompt_seq: Option<i64> = match conn
            .query_row(
                "SELECT MAX(seq) FROM acp_events
                 WHERE session_id = ?1
                   AND json_extract(event_json, '$.UserPromptSent') IS NOT NULL",
                params![session_id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()
        {
            Ok(Some(v)) => v,
            Ok(None) => None,
            Err(e) => {
                warn!(target: "acp.event_store", "has_in_flight_turn prompt query {session_id}: {e}");
                return false;
            }
        };
        let Some(prompt_seq) = prompt_seq else {
            return false;
        };
        let terminator: Option<i64> = match conn
            .query_row(
                "SELECT MIN(seq) FROM acp_events
                 WHERE session_id = ?1
                   AND seq > ?2
                   AND (json_extract(event_json, '$.Stopped') IS NOT NULL
                     OR json_extract(event_json, '$.AgentStartupError') IS NOT NULL)",
                params![session_id, prompt_seq],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()
        {
            Ok(Some(v)) => v,
            Ok(None) => None,
            Err(e) => {
                warn!(target: "acp.event_store", "has_in_flight_turn terminator query {session_id}: {e}");
                return false;
            }
        };
        if terminator.is_none() {
            return true;
        }
        // An async background agent (claude `Agent` tool with `isAsync`) runs
        // off-protocol and outlives the turn's terminal Stopped: its work is
        // reported only via BackgroundAgent{Launched,Progress,Completed}
        // events. Treat the session as in-flight while any launched or
        // progressing agent has no matching Completed, so a build-stale
        // respawn does not interrupt it mid-work and drop its transcript.
        // Bounded two ways so a lost tailer cannot pin the probe forever:
        // every live tailer emits a terminal BackgroundAgentCompleted
        // (including stalled / error), AND an agent whose latest progress is
        // older than BACKGROUND_AGENT_STALE_AFTER_MS stops counting (its
        // tailer died, e.g. a daemon crash, so no Completed will ever come).
        // See #2573.
        let stale_cutoff = chrono::Utc::now().timestamp_millis() - BACKGROUND_AGENT_STALE_AFTER_MS;
        let bg_in_flight: i64 = match conn
            .query_row(
                "SELECT COUNT(*) FROM (
                     SELECT aid, MAX(created_at) AS last_at FROM (
                         SELECT json_extract(event_json, '$.BackgroundAgentLaunched.agent_id') AS aid,
                                created_at
                           FROM acp_events WHERE session_id = ?1
                             AND json_extract(event_json, '$.BackgroundAgentLaunched') IS NOT NULL
                         UNION ALL
                         SELECT json_extract(event_json, '$.BackgroundAgentProgress.agent_id'),
                                created_at
                           FROM acp_events WHERE session_id = ?1
                             AND json_extract(event_json, '$.BackgroundAgentProgress') IS NOT NULL
                     )
                     WHERE aid IS NOT NULL
                     GROUP BY aid
                 ) started
                 WHERE started.last_at >= ?2
                   AND started.aid NOT IN (
                     SELECT json_extract(event_json, '$.BackgroundAgentCompleted.agent_id')
                       FROM acp_events WHERE session_id = ?1
                         AND json_extract(event_json, '$.BackgroundAgentCompleted') IS NOT NULL
                   )",
                params![session_id, stale_cutoff],
                |row| row.get::<_, i64>(0),
            )
            .optional()
        {
            Ok(Some(v)) => v,
            Ok(None) => 0,
            Err(e) => {
                warn!(target: "acp.event_store", "has_in_flight_turn bg-agent query {session_id}: {e}");
                0
            }
        };
        bg_in_flight > 0
    }

    /// Latest `created_at` (ms since epoch) per session for the given
    /// ids, in a single grouped query. Sessions with no events are
    /// absent from the returned map. Backed by the
    /// `(session_id, created_at)` index. Used by the reconciler's
    /// idle-reap pass (#1689) to find structured view workers that have seen no
    /// activity for longer than `acp.auto_stop_idle_secs`, without a
    /// per-session round trip.
    pub fn last_event_at_for_sessions(
        &self,
        session_ids: &[String],
    ) -> std::collections::HashMap<String, i64> {
        // Exclude non-substantive lifecycle/metadata events so they do not
        // reset the idle clock. AcpSessionAssigned in particular is emitted
        // on every cold-start resume (acp_client.rs), so counting it would
        // make a daemon restart look like fresh activity for every worker
        // and the idle-reap (#1689) would never fire across restarts. Shares
        // NON_SUBSTANTIVE_EVENT_DISCRIMINANTS with the retention prune so the
        // two predicates cannot desync.
        if session_ids.is_empty() {
            return std::collections::HashMap::new();
        }
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        events::last_event_at_for_topics(
            &conn,
            &self.schema,
            session_ids,
            NON_SUBSTANTIVE_EVENT_DISCRIMINANTS,
        )
    }

    /// Persist one decoded attachment blob, keyed to the seq of the
    /// `UserPromptSent` event it rides with so the retention prune and
    /// `delete_session` can drop it in lockstep with that event. Called
    /// from the prompt handler before the event is published, so a
    /// client that receives the `UserPromptSent` can immediately fetch
    /// the blob over the GET endpoint. Idempotent on
    /// `(session_id, attachment_id)`.
    /// Returns `true` on success. On failure the caller must abort
    /// publishing the matching `UserPromptSent` and roll back any blobs
    /// already recorded for this seq, so attachment refs never point at
    /// rows `load_attachment()` cannot serve.
    pub fn record_attachment(&self, session_id: &str, seq: u64, blob: &AttachmentBlob) -> bool {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        events::insert_attachment(
            &conn,
            &self.schema,
            session_id,
            seq,
            &blob.id,
            blob.kind.as_str(),
            &blob.mime_type,
            blob.name.as_deref(),
            &blob.data,
            now_ms,
        )
    }

    /// Drop all attachment blobs owned by one prompt seq. Used as a
    /// rollback when `UserPromptSent` could not be durably persisted, so
    /// attachment refs and blobs stay in sync.
    pub fn delete_attachments_for_seq(&self, session_id: &str, seq: u64) {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        events::delete_attachments_for_seq(&conn, &self.schema, session_id, seq);
    }

    /// Fetch one attachment's MIME type and bytes for the replay GET
    /// endpoint. Scoped by `session_id` so a valid token for one session
    /// can't read another session's blob by guessing ids.
    pub fn load_attachment(
        &self,
        session_id: &str,
        attachment_id: &str,
    ) -> Option<(String, Vec<u8>)> {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        events::load_attachment(&conn, &self.schema, session_id, attachment_id)
    }

    /// Latest prompt capabilities the agent advertised for this session,
    /// or `None` if no `PromptCapabilities` event is on disk yet. Read by
    /// the prompt handler to reject attachments the agent cannot accept,
    /// mirroring `latest_plan`. Returns `(image, audio, embedded_context)`.
    pub fn latest_prompt_capabilities(&self, session_id: &str) -> Option<(bool, bool, bool)> {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let (_, json) =
            events::latest_by_discriminant(&conn, &self.schema, session_id, "PromptCapabilities")?;
        match serde_json::from_str::<Event>(&json).ok()? {
            Event::PromptCapabilities {
                image,
                audio,
                embedded_context,
            } => Some((image, audio, embedded_context)),
            _ => None,
        }
    }

    /// Drop every event for a session. Called when the session is
    /// deleted or its view is switched away from structured view, so the
    /// next acp_enable starts fresh from seq=1.
    pub fn delete_session(&self, session_id: &str) {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        // Cascades to attachment blobs so a deleted session leaves no
        // orphaned bytes behind.
        let deleted = events::delete_topic(&conn, &self.schema, session_id);
        debug!(
            target: "acp.event_store",
            session = %session_id,
            deleted,
            "deleted session events"
        );
    }
}

/// A decoded attachment ready to persist: the storage-side counterpart
/// of the wire `PromptAttachmentUpload`. Holds raw bytes (not base64) so
/// the GET endpoint serves them directly. The matching metadata-only
/// `PromptAttachmentRef` is what rides in the event log.
pub struct AttachmentBlob {
    pub id: String,
    pub kind: crate::acp::state::PromptAttachmentKind,
    pub mime_type: String,
    pub name: Option<String>,
    pub data: Vec<u8>,
}

/// Cheap discriminant string for `Event` so debug logs don't dump the
/// full payload (assistant chunks can be a few KB each). Unknown
/// variants fall back to "other"; `event_kind` only exists for log
/// breadcrumbs and doesn't need to stay in lockstep with the enum.
/// Shortest query `search_content` will run; below this every session
/// matches and the result is noise.
const MIN_SEARCH_CHARS: usize = 2;
/// Cap on query length so a pathological input can't build a huge LIKE
/// pattern.
const MAX_SEARCH_CHARS: usize = 128;
/// Most sessions returned from a single search.
const MAX_SEARCH_RESULTS: usize = 20;
/// Hard ceiling on rows the LIKE prefilter scans, so a no-match query on a
/// huge DB can't scan the whole table. The scan also stops early as soon as
/// `limit` distinct sessions are collected, so the common case reads far
/// fewer rows; this ceiling only bites when matches are sparse. Set well
/// above `limit` so a single chatty session's run of newest events does not
/// crowd other matching sessions out of the window.
const SEARCH_ROW_SCAN_CAP: usize = 2000;
/// Characters of context kept on each side of the match in a snippet.
const SNIPPET_RADIUS_CHARS: usize = 60;

/// One session's content-search hit: the newest matching event plus a
/// count of how many of its events matched within the scanned window.
#[derive(Debug, Clone)]
pub struct ContentHit {
    pub session_id: String,
    pub seq: u64,
    pub kind: &'static str,
    pub snippet: String,
    pub match_count: usize,
}

/// Escape the LIKE metacharacters so user input is matched literally
/// rather than as wildcard syntax (a bare `%` would otherwise match
/// every row). Paired with `ESCAPE '\'` in the query.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Extract the user-facing prose from an event for content search, with
/// a short kind label. Returns `None` for structural / metadata events
/// that carry no conversation text. Only the high-signal prose sources
/// are indexed; status and error strings are deliberately excluded so a
/// search stays scoped to the conversation.
fn event_search_text(event: &Event) -> Option<(&'static str, String)> {
    let pick = |s: &str, kind: &'static str| {
        let t = s.trim();
        if t.is_empty() {
            None
        } else {
            Some((kind, s.to_string()))
        }
    };
    match event {
        Event::AgentMessageChunk { text, .. } => pick(text, "agent"),
        Event::UserPromptSent { text, .. } => pick(text, "user"),
        Event::UserDiffCommentsPrompt {
            assembled_markdown, ..
        } => pick(assembled_markdown, "user"),
        Event::ToolCallContent { content, .. } => pick(content, "tool"),
        Event::ToolCallCompleted { content, .. } => pick(content, "tool"),
        _ => None,
    }
}

/// Build a display snippet: a window of `SNIPPET_RADIUS_CHARS` on each
/// side of the first case-insensitive match of `needle` (already
/// lowercased) in `text`, whitespace-collapsed, with ellipses where
/// trimmed. Falls back to the head of the text when the needle is not
/// found (callers only pass text that matched, so this is rare). UTF-8
/// safe: windows on char boundaries, never byte offsets.
fn make_snippet(text: &str, needle: &str) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = collapsed.chars().collect();
    let lower = collapsed.to_lowercase();
    // Map the byte index of the match to a char index over `chars`.
    let match_char = lower
        .find(needle)
        .map(|byte_idx| lower[..byte_idx].chars().count())
        .unwrap_or(0);
    let start = match_char.saturating_sub(SNIPPET_RADIUS_CHARS);
    let end = (match_char + needle.chars().count() + SNIPPET_RADIUS_CHARS).min(chars.len());
    let mut snippet = String::new();
    if start > 0 {
        snippet.push('…');
    }
    snippet.extend(&chars[start..end]);
    if end < chars.len() {
        snippet.push('…');
    }
    snippet
}

fn event_kind(event: &Event) -> &'static str {
    match event {
        Event::PlanUpdated { .. } => "plan_updated",
        Event::TodoListUpdated { .. } => "todo_list_updated",
        Event::SessionTitleSuggested { .. } => "session_title_suggested",
        Event::ToolCallStarted { .. } => "tool_call_started",
        Event::ToolCallCompleted { .. } => "tool_call_completed",
        Event::ToolCallContent { .. } => "tool_call_content",
        Event::ToolCallUpdated { .. } => "tool_call_updated",
        Event::ApprovalRequested { .. } => "approval_requested",
        Event::ApprovalResolved { .. } => "approval_resolved",
        Event::ElicitationRequested { .. } => "elicitation_requested",
        Event::ElicitationResolved { .. } => "elicitation_resolved",
        Event::DiffEmitted { .. } => "diff_emitted",
        Event::ThinkingStarted => "thinking_started",
        Event::ThinkingEnded => "thinking_ended",
        Event::RateLimit { .. } => "rate_limit",
        Event::RateLimitAutoResumed { .. } => "rate_limit_auto_resumed",
        Event::UsageUpdated { .. } => "usage_updated",
        Event::ModeChanged { .. } => "mode_changed",
        Event::ModesAvailable { .. } => "modes_available",
        Event::CurrentModeChanged { .. } => "current_mode_changed",
        Event::ModeSwitchFailed { .. } => "mode_switch_failed",
        Event::AvailableCommandsUpdated { .. } => "available_commands_updated",
        Event::ConfigOptionsUpdated { .. } => "config_options_updated",
        Event::ConfigOptionSwitchFailed { .. } => "config_option_switch_failed",
        Event::RawAgentUpdate { .. } => "raw_agent_update",
        Event::BackgroundAgentLaunched { .. } => "background_agent_launched",
        Event::BackgroundAgentProgress { .. } => "background_agent_progress",
        Event::BackgroundAgentCompleted { .. } => "background_agent_completed",
        Event::PromptRuntimeError { .. } => "prompt_runtime_error",
        Event::AgentMessageChunk { .. } => "agent_message_chunk",
        Event::CancelRequested { .. } => "cancel_requested",
        Event::Stopped { .. } => "stopped",
        Event::AgentStartupError { .. } => "agent_startup_error",
        Event::IncompatibleAgent { .. } => "incompatible_agent",
        Event::UserPromptSent { .. } => "user_prompt_sent",
        Event::UserDiffCommentsPrompt { .. } => "user_diff_comments_prompt",
        Event::PromptCapabilities { .. } => "prompt_capabilities",
        Event::AcpSessionAssigned { .. } => "acp_session_assigned",
        Event::SessionContextReset { .. } => "session_context_reset",
        Event::SessionCleared => "session_cleared",
        Event::ConversationCompacted => "conversation_compacted",
        Event::WakeupScheduled { .. } => "wakeup_scheduled",
        Event::MonitorArmed { .. } => "monitor_armed",
        Event::PromptRejected { .. } => "prompt_rejected",
        Event::AgentSwitched { .. } => "agent_switched",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn open_store(max: usize) -> (TempDir, EventStore) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("acp.db");
        let store = EventStore::open(&path, max).unwrap();
        (tmp, store)
    }

    fn agent_chunk(text: &str) -> Event {
        Event::AgentMessageChunk { text: text.into() }
    }

    fn user_prompt(text: &str) -> Event {
        Event::UserPromptSent {
            text: text.into(),
            attachments: vec![],
        }
    }

    #[test]
    fn search_content_finds_prompt_agent_and_tool_text() {
        let (_tmp, store) = open_store(1000);
        store
            .record("s1", 1, &user_prompt("please refactor the reconciler"))
            .unwrap();
        store
            .record("s2", 1, &agent_chunk("I updated the supervisor"))
            .unwrap();
        store
            .record(
                "s3",
                1,
                &Event::ToolCallContent {
                    tool_call_id: "t1".into(),
                    content: "grep matched reconciler in supervisor.rs".into(),
                },
            )
            .unwrap();

        let hits = store.search_content("reconciler", 10);
        let ids: Vec<&str> = hits.iter().map(|h| h.session_id.as_str()).collect();
        assert!(ids.contains(&"s1"), "prompt text should match");
        assert!(ids.contains(&"s3"), "tool content should match");
        assert!(!ids.contains(&"s2"), "non-matching session excluded");
    }

    #[test]
    fn search_content_ignores_json_keys() {
        let (_tmp, store) = open_store(1000);
        // The substring "text" is a JSON key in every UserPromptSent row,
        // so a raw LIKE would match; the decode-and-verify step must drop
        // it because the prose does not contain "text".
        store.record("s1", 1, &user_prompt("hello world")).unwrap();
        let hits = store.search_content("text", 10);
        assert!(hits.is_empty(), "JSON key 'text' must not match prose");
    }

    #[test]
    fn search_content_is_case_insensitive_and_groups_by_session() {
        let (_tmp, store) = open_store(1000);
        store
            .record("s1", 1, &agent_chunk("The Quick Brown Fox"))
            .unwrap();
        store.record("s1", 2, &agent_chunk("quick again")).unwrap();
        let hits = store.search_content("QUICK", 10);
        assert_eq!(hits.len(), 1, "two matching events collapse to one session");
        assert_eq!(hits[0].session_id, "s1");
        assert_eq!(hits[0].match_count, 2);
    }

    #[test]
    fn search_content_escapes_like_wildcards() {
        let (_tmp, store) = open_store(1000);
        store
            .record("s1", 1, &agent_chunk("plain message"))
            .unwrap();
        // Unescaped, "e%" becomes LIKE pattern %e%% which matches any text
        // containing "e" (the prose has several). Escaped, it matches only
        // the literal substring "e%", which the prose lacks, so the result
        // is empty. This fails if wildcard escaping regresses.
        let hits = store.search_content("e%", 10);
        assert!(
            hits.is_empty(),
            "% must be matched literally, not as a wildcard"
        );
    }

    #[test]
    fn search_content_caps_distinct_sessions_not_raw_rows() {
        let (_tmp, store) = open_store(1000);
        // A chatty session (s_busy) with many matching events recorded first
        // (older), then a single newer match in s_quiet. The cap is on
        // distinct sessions, so s_busy must not crowd s_quiet out.
        for seq in 1..=20 {
            store
                .record("s_busy", seq, &agent_chunk("needle again"))
                .unwrap();
        }
        store.record("s_quiet", 1, &agent_chunk("needle")).unwrap();

        let all = store.search_content("needle", 10);
        let ids: Vec<&str> = all.iter().map(|h| h.session_id.as_str()).collect();
        assert!(ids.contains(&"s_busy"), "chatty session present");
        assert!(
            ids.contains(&"s_quiet"),
            "quiet session not hidden by chatty one"
        );

        // limit caps the number of distinct sessions, not raw matched rows.
        let one = store.search_content("needle", 1);
        assert_eq!(one.len(), 1, "limit bounds distinct sessions");
    }

    fn img_blob(id: &str) -> AttachmentBlob {
        AttachmentBlob {
            id: id.to_string(),
            kind: crate::acp::state::PromptAttachmentKind::Image,
            mime_type: "image/png".into(),
            name: Some("shot.png".into()),
            data: vec![0x89, 0x50, 0x4E, 0x47, 1, 2, 3],
        }
    }

    fn prompt_with_attachment(id: &str) -> Event {
        Event::UserPromptSent {
            text: "look at this".into(),
            attachments: vec![crate::acp::state::PromptAttachmentRef {
                id: id.to_string(),
                kind: crate::acp::state::PromptAttachmentKind::Image,
                mime_type: "image/png".into(),
                name: Some("shot.png".into()),
                size: 7,
            }],
        }
    }

    #[test]
    fn first_user_prompt_returns_earliest_prompt_text() {
        let (_tmp, store) = open_store(1000);
        let prompt = |text: &str| Event::UserPromptSent {
            text: text.into(),
            attachments: vec![],
        };
        // A session with no prompt yet => None (manual rename has nothing to
        // name from).
        assert!(store.first_user_prompt("s-1").is_none());
        // Earliest UserPromptSent by seq wins, even when recorded out of order.
        store.record("s-1", 2, &prompt("second prompt")).unwrap();
        store.record("s-1", 1, &prompt("first prompt")).unwrap();
        assert_eq!(
            store.first_user_prompt("s-1").as_deref(),
            Some("first prompt")
        );
        // Scoped per session.
        assert!(store.first_user_prompt("s-2").is_none());
    }

    #[test]
    fn attachment_record_and_load_roundtrip() {
        let (_tmp, store) = open_store(1000);
        store
            .record("s-1", 1, &prompt_with_attachment("a1"))
            .unwrap();
        store.record_attachment("s-1", 1, &img_blob("a1"));
        let (mime, bytes) = store.load_attachment("s-1", "a1").expect("blob present");
        assert_eq!(mime, "image/png");
        assert_eq!(bytes, vec![0x89, 0x50, 0x4E, 0x47, 1, 2, 3]);
        // Wrong session must not read another session's blob by id.
        assert!(store.load_attachment("s-2", "a1").is_none());
        // Unknown id is None.
        assert!(store.load_attachment("s-1", "nope").is_none());
    }

    #[test]
    fn attachment_pruned_with_owning_prompt() {
        // The user's explicit requirement: the attachment table must
        // not grow without bound. When the retention cap evicts the
        // prompt event an attachment rode with, the blob must go too.
        let (_tmp, store) = open_store(3);
        store
            .record("s-1", 1, &prompt_with_attachment("a1"))
            .unwrap();
        store.record_attachment("s-1", 1, &img_blob("a1"));
        assert!(store.load_attachment("s-1", "a1").is_some());
        // Blow past the cap so seq 1 is pruned.
        for i in 2..=20 {
            store.record("s-1", i, &Event::ThinkingStarted).unwrap();
        }
        assert!(
            store.load_attachment("s-1", "a1").is_none(),
            "attachment blob outlived its pruned prompt event",
        );
    }

    #[test]
    fn delete_session_clears_attachments() {
        let (_tmp, store) = open_store(1000);
        store
            .record("s-1", 1, &prompt_with_attachment("a1"))
            .unwrap();
        store.record_attachment("s-1", 1, &img_blob("a1"));
        store
            .record("s-2", 1, &prompt_with_attachment("b1"))
            .unwrap();
        store.record_attachment("s-2", 1, &img_blob("b1"));
        store.delete_session("s-1");
        assert!(store.load_attachment("s-1", "a1").is_none());
        // Sibling session untouched.
        assert!(store.load_attachment("s-2", "b1").is_some());
    }

    #[test]
    fn latest_prompt_capabilities_reads_most_recent() {
        let (_tmp, store) = open_store(1000);
        assert_eq!(store.latest_prompt_capabilities("s-1"), None);
        store
            .record(
                "s-1",
                1,
                &Event::PromptCapabilities {
                    image: true,
                    audio: false,
                    embedded_context: true,
                },
            )
            .unwrap();
        assert_eq!(
            store.latest_prompt_capabilities("s-1"),
            Some((true, false, true))
        );
        // A later capabilities event (e.g. after agent switch) wins.
        store
            .record(
                "s-1",
                2,
                &Event::PromptCapabilities {
                    image: false,
                    audio: false,
                    embedded_context: false,
                },
            )
            .unwrap();
        assert_eq!(
            store.latest_prompt_capabilities("s-1"),
            Some((false, false, false))
        );
    }

    #[test]
    fn user_prompt_event_deserialises_without_attachments_field() {
        // Back-compat: events written before this feature have no
        // `attachments` key. `#[serde(default)]` must hydrate them as
        // text-only rather than failing the whole replay.
        let json = r#"{"UserPromptSent":{"text":"legacy"}}"#;
        let event: Event = serde_json::from_str(json).expect("legacy event deserialises");
        match event {
            Event::UserPromptSent { text, attachments } => {
                assert_eq!(text, "legacy");
                assert!(attachments.is_empty());
            }
            other => panic!("expected UserPromptSent, got {other:?}"),
        }
    }

    #[test]
    fn record_and_replay_roundtrip() {
        let (_tmp, store) = open_store(1000);
        for i in 1..=5 {
            store.record("s-1", i, &Event::ThinkingStarted).unwrap();
        }
        let replay = store.replay_from("s-1", 2);
        let seqs: Vec<u64> = replay.iter().map(|(s, _)| *s).collect();
        assert_eq!(seqs, vec![3, 4, 5]);
    }

    #[test]
    fn last_event_at_ignores_non_substantive_events() {
        // #1689: the idle clock must reflect real activity, not lifecycle /
        // metadata events. A session whose only events are AcpSessionAssigned
        // (emitted on every cold-start resume), ModesAvailable, etc. must NOT
        // register a recent activity timestamp, otherwise a daemon restart
        // would reset every worker's idle timer and the reap would never fire.
        let (_tmp, store) = open_store(1000);
        store
            .record(
                "s-lifecycle",
                1,
                &Event::AcpSessionAssigned {
                    acp_session_id: "acp-1".into(),
                },
            )
            .unwrap();
        store
            .record(
                "s-lifecycle",
                2,
                &Event::ModesAvailable {
                    current_mode_id: "default".into(),
                    modes: vec![],
                },
            )
            .unwrap();
        store
            .record(
                "s-real",
                1,
                &Event::UserPromptSent {
                    text: "hello".into(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();

        let map =
            store.last_event_at_for_sessions(&["s-lifecycle".to_string(), "s-real".to_string()]);
        assert!(
            !map.contains_key("s-lifecycle"),
            "non-substantive-only session must not register activity: {map:?}"
        );
        assert!(
            map.contains_key("s-real"),
            "session with a real event must register activity: {map:?}"
        );
    }

    /// Insert a substantive event with an explicit `created_at` (ms) so
    /// tests can pin MAX(created_at) deterministically. `"ThinkingStarted"`
    /// serializes to a bare JSON string, so it never matches the
    /// non-substantive `{"Name":%` predicates.
    fn insert_at(store: &EventStore, session_id: &str, seq: u64, created_at: i64) {
        let conn = store.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO acp_events (session_id, seq, event_json, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![session_id, seq as i64, "\"ThinkingStarted\"", created_at],
        )
        .unwrap();
    }

    #[test]
    fn last_event_at_empty_input_is_empty() {
        let (_tmp, store) = open_store(1000);
        insert_at(&store, "s-1", 1, 1000);
        assert!(store.last_event_at_for_sessions(&[]).is_empty());
    }

    #[test]
    fn last_event_at_skips_missing_sessions() {
        let (_tmp, store) = open_store(1000);
        insert_at(&store, "s-present", 1, 5000);
        let map =
            store.last_event_at_for_sessions(&["s-present".to_string(), "s-missing".to_string()]);
        assert_eq!(map.get("s-present"), Some(&5000));
        assert!(!map.contains_key("s-missing"));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn last_event_at_returns_max_per_session() {
        let (_tmp, store) = open_store(1000);
        insert_at(&store, "s-a", 1, 100);
        insert_at(&store, "s-a", 2, 900);
        insert_at(&store, "s-a", 3, 400);
        insert_at(&store, "s-b", 1, 7000);
        let map = store.last_event_at_for_sessions(&["s-a".to_string(), "s-b".to_string()]);
        assert_eq!(map.get("s-a"), Some(&900));
        assert_eq!(map.get("s-b"), Some(&7000));
    }

    #[test]
    fn replay_page_reassembles_into_unbounded_result() {
        // Paging with a small limit and following `last_scanned_seq`
        // must rebuild exactly what a single unbounded replay returns.
        let (_tmp, store) = open_store(1000);
        for i in 1..=10 {
            store.record("s-1", i, &Event::ThinkingStarted).unwrap();
        }
        let unbounded: Vec<u64> = store
            .replay_from("s-1", 0)
            .iter()
            .map(|(s, _)| *s)
            .collect();

        let mut paged = Vec::new();
        let mut cursor = 0u64;
        loop {
            let page = store.replay_page("s-1", cursor, Some(3));
            assert!(page.events.len() <= 3, "page exceeded limit");
            paged.extend(page.events.iter().map(|(s, _)| *s));
            match page.last_scanned_seq {
                Some(next) if page.has_more => cursor = next,
                _ => break,
            }
        }
        assert_eq!(paged, unbounded);
    }

    #[test]
    fn replay_page_has_more_at_exact_boundary() {
        let (_tmp, store) = open_store(1000);
        for i in 1..=4 {
            store.record("s-1", i, &Event::ThinkingStarted).unwrap();
        }
        // Limit equal to the row count: full page, nothing left over.
        let full = store.replay_page("s-1", 0, Some(4));
        assert_eq!(full.events.len(), 4);
        assert!(!full.has_more);
        assert_eq!(full.last_scanned_seq, Some(4));

        // One short: a probe row remains, so `has_more` is set and the
        // cursor stops at the last consumed seq, not the probe row.
        let partial = store.replay_page("s-1", 0, Some(3));
        assert_eq!(partial.events.len(), 3);
        assert!(partial.has_more);
        assert_eq!(partial.last_scanned_seq, Some(3));
    }

    #[test]
    fn replay_page_cursor_advances_past_corrupt_row() {
        // A row that fails to deserialise is skipped from `events` but
        // still advances `last_scanned_seq`, so a paging loop can never
        // get stuck re-requesting the same corrupt seq.
        let (_tmp, store) = open_store(1000);
        store.record("s-1", 1, &Event::ThinkingStarted).unwrap();
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO acp_events (session_id, seq, event_json, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params!["s-1", 2_i64, "{not valid event json", 0_i64],
            )
            .unwrap();
        }
        store.record("s-1", 3, &Event::ThinkingEnded).unwrap();

        // Page size 2 lands the corrupt row mid-page: it is dropped from
        // events but the cursor moves to seq 2, so the next page yields 3.
        let page1 = store.replay_page("s-1", 0, Some(2));
        assert_eq!(
            page1.events.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            vec![1]
        );
        assert_eq!(page1.last_scanned_seq, Some(2));
        assert!(page1.has_more);

        let page2 = store.replay_page("s-1", page1.last_scanned_seq.unwrap(), Some(2));
        assert_eq!(
            page2.events.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            vec![3]
        );
        assert!(!page2.has_more);
    }

    #[test]
    fn replay_page_since_u64_max_returns_empty() {
        // The status probe passes `since = u64::MAX` to read metadata
        // only. Without clamping, `u64::MAX as i64` is -1 and `seq > -1`
        // would return the whole transcript. It must return no rows but
        // still report the bounds.
        let (_tmp, store) = open_store(1000);
        for i in 1..=3 {
            store.record("s-1", i, &Event::ThinkingStarted).unwrap();
        }
        let page = store.replay_page("s-1", u64::MAX, Some(1000));
        assert!(page.events.is_empty());
        assert!(!page.has_more);
        assert_eq!(page.highest_seq, 3);
        assert_eq!(page.lowest_seq, Some(1));
    }

    #[test]
    fn highest_seq_reflects_inserts() {
        let (_tmp, store) = open_store(1000);
        assert_eq!(store.highest_seq("s-1"), 0);
        store.record("s-1", 1, &Event::ThinkingStarted).unwrap();
        store.record("s-1", 2, &Event::ThinkingEnded).unwrap();
        assert_eq!(store.highest_seq("s-1"), 2);
    }

    #[test]
    fn lowest_seq_none_on_empty() {
        let (_tmp, store) = open_store(1000);
        assert_eq!(store.lowest_seq("s-1"), None);
    }

    #[test]
    fn lowest_seq_reflects_oldest_remaining_seq() {
        let (_tmp, store) = open_store(1000);
        store.record("s-1", 5, &Event::ThinkingStarted).unwrap();
        store.record("s-1", 7, &Event::ThinkingEnded).unwrap();
        assert_eq!(store.lowest_seq("s-1"), Some(5));
    }

    #[test]
    fn lowest_seq_climbs_with_retention_prune() {
        // After the retention prune evicts the early transcript seqs,
        // `lowest_seq` must reflect the new floor so callers can detect
        // a client `since` cursor that's fallen below it.
        let (_tmp, store) = open_store(3);
        for i in 1..=20 {
            store.record("s-1", i, &Event::ThinkingStarted).unwrap();
        }
        // Cap is 3 transcript events; with no snapshot rows, only seqs
        // 18, 19, 20 remain.
        let low = store.lowest_seq("s-1").expect("some events stored");
        assert!(low > 1, "lowest_seq did not advance after prune: {low}");
    }

    #[test]
    fn duplicate_seq_is_idempotent() {
        let (_tmp, store) = open_store(1000);
        store
            .record(
                "s-1",
                1,
                &Event::UserPromptSent {
                    text: "hi".into(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        // Second insert at the same seq must not double-count.
        store.record("s-1", 1, &Event::ThinkingStarted).unwrap();
        let replay = store.replay_from("s-1", 0);
        assert_eq!(replay.len(), 1);
        // The first write wins (INSERT OR IGNORE).
        if let Event::UserPromptSent { text, .. } = &replay[0].1 {
            assert_eq!(text, "hi");
        } else {
            panic!("expected UserPromptSent");
        }
    }

    #[test]
    fn diff_comments_prompt_round_trips_structured_fields() {
        use super::super::state::DiffComment;
        let (_tmp, store) = open_store(1000);
        let comment = DiffComment {
            id: "c-1".into(),
            repo_name: Some("repoA".into()),
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
        store
            .record(
                "s-1",
                1,
                &Event::UserDiffCommentsPrompt {
                    intro: "Hey:".into(),
                    outro: "Please address these comments.".into(),
                    is_multi_repo: true,
                    comments: vec![comment],
                    assembled_markdown: "## Diff comments\n\n...".into(),
                },
            )
            .unwrap();
        let replay = store.replay_from("s-1", 0);
        assert_eq!(replay.len(), 1);
        match &replay[0].1 {
            Event::UserDiffCommentsPrompt {
                intro,
                is_multi_repo,
                comments,
                assembled_markdown,
                ..
            } => {
                assert_eq!(intro, "Hey:");
                assert!(*is_multi_repo);
                assert_eq!(comments.len(), 1);
                assert_eq!(comments[0].repo_name.as_deref(), Some("repoA"));
                assert_eq!(comments[0].start_line, 42);
                assert!(assembled_markdown.starts_with("## Diff comments"));
            }
            other => panic!("expected UserDiffCommentsPrompt, got {other:?}"),
        }
    }

    #[test]
    fn diff_comments_prompt_serialises_camel_case() {
        use super::super::state::DiffComment;
        let event = Event::UserDiffCommentsPrompt {
            intro: String::new(),
            outro: "Please address these comments.".into(),
            is_multi_repo: false,
            comments: vec![DiffComment {
                id: "c-1".into(),
                repo_name: None,
                file_path: "a.rs".into(),
                side: "old".into(),
                start_line: 1,
                end_line: 1,
                body: "b".into(),
                captured_snippet: "s".into(),
                language: None,
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: None,
            }],
            assembled_markdown: "m".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        // Event payload fields and comment fields are both camelCase on
        // the wire so the frontend union mirrors them one-for-one.
        assert!(json.contains("\"isMultiRepo\""));
        assert!(json.contains("\"assembledMarkdown\""));
        assert!(json.contains("\"filePath\""));
        assert!(json.contains("\"startLine\""));
        // repo_name / updated_at are skipped when None.
        assert!(!json.contains("\"repoName\""));
        assert!(!json.contains("\"updatedAt\""));
    }

    #[test]
    fn latest_plan_returns_most_recent_plan_event() {
        use super::super::state::{Plan, PlanStep, PlanStepStatus};
        let (_tmp, store) = open_store(1000);
        let plan_v1 = Plan {
            plan_id: "p-1".into(),
            version: 1,
            steps: vec![PlanStep {
                id: "s-1".into(),
                title: "Step one".into(),
                detail: None,
                status: PlanStepStatus::Pending,
            }],
        };
        let plan_v2 = Plan {
            plan_id: "p-2".into(),
            version: 2,
            steps: vec![
                PlanStep {
                    id: "s-1".into(),
                    title: "Step one".into(),
                    detail: None,
                    status: PlanStepStatus::Done,
                },
                PlanStep {
                    id: "s-2".into(),
                    title: "Step two".into(),
                    detail: None,
                    status: PlanStepStatus::Pending,
                },
            ],
        };
        store
            .record("s-1", 1, &Event::PlanUpdated { plan: plan_v1 })
            .unwrap();
        store.record("s-1", 2, &Event::ThinkingStarted).unwrap();
        store
            .record("s-1", 3, &Event::PlanUpdated { plan: plan_v2 })
            .unwrap();
        let latest = store.latest_plan("s-1").expect("plan present");
        assert_eq!(latest.steps.len(), 2);
        assert!(matches!(
            latest.steps[0].status,
            crate::acp::state::PlanStepStatus::Done
        ));
    }

    #[test]
    fn latest_plan_returns_none_when_no_plan_event() {
        let (_tmp, store) = open_store(1000);
        store.record("s-1", 1, &Event::ThinkingStarted).unwrap();
        assert!(store.latest_plan("s-1").is_none());
    }

    #[test]
    fn snapshot_events_survive_retention_prune() {
        // Mirrors #1049: a long session blew past max_events_per_session
        // and evicted the early `AvailableCommandsUpdated` row, leaving
        // the `/` palette empty on reconnect. Snapshot kinds are pinned
        // so they outlive the prune even when the rest of the seq tail
        // gets dropped.
        let (_tmp, store) = open_store(3);
        store
            .record(
                "s-1",
                1,
                &Event::AvailableCommandsUpdated { commands: vec![] },
            )
            .unwrap();
        store
            .record(
                "s-1",
                2,
                &Event::ModesAvailable {
                    current_mode_id: "default".into(),
                    modes: vec![],
                },
            )
            .unwrap();
        store
            .record(
                "s-1",
                3,
                &Event::AcpSessionAssigned {
                    acp_session_id: "acp-xyz".into(),
                },
            )
            .unwrap();
        // Push enough transcript events to blow past the cap several
        // times. With the old prune, seqs 1-3 would all be evicted.
        for i in 4..=20 {
            store.record("s-1", i, &Event::ThinkingStarted).unwrap();
        }
        let replay = store.replay_from("s-1", 0);
        let seqs: Vec<u64> = replay.iter().map(|(s, _)| *s).collect();
        // The three snapshot rows survive. The most recent 3 transcript
        // events also remain.
        assert!(
            seqs.contains(&1),
            "AvailableCommandsUpdated dropped: {seqs:?}"
        );
        assert!(seqs.contains(&2), "ModesAvailable dropped: {seqs:?}");
        assert!(seqs.contains(&3), "AcpSessionAssigned dropped: {seqs:?}");
        assert!(seqs.contains(&20), "newest event dropped: {seqs:?}");
        // Older transcript-only events (4 through 17) are pruned.
        assert!(
            !seqs.contains(&5),
            "stale transcript event leaked: {seqs:?}"
        );
    }

    #[test]
    fn snapshot_event_json_discriminators_match_prune_clauses() {
        // The retention prune query in `Self::record` excludes four event
        // variants via `WHERE event_json NOT LIKE '{"<Variant>":%'`. If the
        // `Event` enum is ever refactored to a different serde shape
        // (`#[serde(tag = "...")]`, a rename, or another adjacency), the
        // LIKE strings silently stop matching and snapshot pinning quietly
        // breaks. Pin the discriminator at the JSON level so any such
        // refactor trips this test instead of going unnoticed.
        let cases: &[(Event, &str)] = &[
            (
                Event::AvailableCommandsUpdated { commands: vec![] },
                "{\"AvailableCommandsUpdated\":",
            ),
            (
                Event::ModesAvailable {
                    current_mode_id: "default".into(),
                    modes: vec![],
                },
                "{\"ModesAvailable\":",
            ),
            (
                Event::CurrentModeChanged {
                    current_mode_id: "default".into(),
                },
                "{\"CurrentModeChanged\":",
            ),
            (
                Event::AcpSessionAssigned {
                    acp_session_id: "acp-xyz".into(),
                },
                "{\"AcpSessionAssigned\":",
            ),
        ];
        for (event, expected_prefix) in cases {
            let json = serde_json::to_string(event).unwrap();
            assert!(
                json.starts_with(expected_prefix),
                "snapshot variant serialised as {json}, expected to start with {expected_prefix}"
            );
        }
    }

    #[test]
    fn retention_cap_drops_oldest() {
        let (_tmp, store) = open_store(3);
        for i in 1..=5 {
            store.record("s-1", i, &Event::ThinkingStarted).unwrap();
        }
        let replay = store.replay_from("s-1", 0);
        let seqs: Vec<u64> = replay.iter().map(|(s, _)| *s).collect();
        // Newest 3 survive: seqs 3, 4, 5. Oldest (1, 2) pruned.
        assert_eq!(seqs, vec![3, 4, 5]);
    }

    #[test]
    fn delete_session_clears_only_target() {
        let (_tmp, store) = open_store(1000);
        store.record("s-1", 1, &Event::ThinkingStarted).unwrap();
        store.record("s-2", 1, &Event::ThinkingEnded).unwrap();
        store.delete_session("s-1");
        assert_eq!(store.highest_seq("s-1"), 0);
        assert_eq!(store.highest_seq("s-2"), 1);
    }

    #[test]
    fn all_session_seqs_lists_each_session_once() {
        let (_tmp, store) = open_store(1000);
        store.record("s-1", 1, &Event::ThinkingStarted).unwrap();
        store.record("s-1", 2, &Event::ThinkingEnded).unwrap();
        store.record("s-2", 1, &Event::ThinkingStarted).unwrap();
        let mut listed = store.all_session_seqs();
        listed.sort();
        assert_eq!(listed, vec![("s-1".to_string(), 2), ("s-2".to_string(), 1)]);
    }

    #[test]
    fn has_in_flight_turn_empty_store_returns_false() {
        let (_tmp, store) = open_store(1000);
        assert!(!store.has_in_flight_turn("s-1"));
    }

    #[test]
    fn has_in_flight_turn_true_while_background_agent_unfinished() {
        // #2573: an async background agent runs after the turn's terminal
        // Stopped. A build-stale respawn must defer until the agent finishes,
        // so the session counts as in-flight while any launched/progressing
        // agent has no matching BackgroundAgentCompleted.
        let (_tmp, store) = open_store(1000);
        store
            .record(
                "s-1",
                1,
                &Event::UserPromptSent {
                    text: "go".into(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        store
            .record(
                "s-1",
                2,
                &Event::Stopped {
                    reason: "prompt_complete".into(),
                },
            )
            .unwrap();
        store
            .record(
                "s-1",
                3,
                &Event::BackgroundAgentProgress {
                    agent_id: "bg-1".into(),
                    status: crate::acp::state::BackgroundAgentStatus::Running,
                    tool_count: 1,
                    tools: Vec::new(),
                    last_tool: None,
                    last_text: None,
                    at: chrono::Utc::now(),
                },
            )
            .unwrap();
        // Turn is stopped, but the background agent has no terminal yet.
        assert!(store.has_in_flight_turn("s-1"));
        store
            .record(
                "s-1",
                4,
                &Event::BackgroundAgentCompleted {
                    agent_id: "bg-1".into(),
                    status: crate::acp::state::BackgroundAgentStatus::Completed,
                    tools: Vec::new(),
                    result: None,
                    warning: None,
                    ended_at: chrono::Utc::now(),
                },
            )
            .unwrap();
        // Its completion drains the in-flight state.
        assert!(!store.has_in_flight_turn("s-1"));
    }

    #[test]
    fn has_in_flight_turn_ignores_stale_background_agent() {
        // #2573: a tailer that died (e.g. daemon crash) leaves a bg agent
        // with Launched/Progress and no Completed. After the staleness window
        // it must stop counting, so a lost tailer cannot pin the build-stale
        // respawn pass forever.
        let (_tmp, store) = open_store(1000);
        store
            .record(
                "s-1",
                1,
                &Event::UserPromptSent {
                    text: "go".into(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        store
            .record(
                "s-1",
                2,
                &Event::Stopped {
                    reason: "prompt_complete".into(),
                },
            )
            .unwrap();
        let stale_at =
            chrono::Utc::now().timestamp_millis() - (BACKGROUND_AGENT_STALE_AFTER_MS + 60_000);
        store
            .record_at(
                "s-1",
                3,
                &Event::BackgroundAgentProgress {
                    agent_id: "bg-1".into(),
                    status: crate::acp::state::BackgroundAgentStatus::Running,
                    tool_count: 1,
                    tools: Vec::new(),
                    last_tool: None,
                    last_text: None,
                    at: chrono::Utc::now(),
                },
                stale_at,
            )
            .unwrap();
        // Progress older than the window with no Completed: treated as gone.
        assert!(!store.has_in_flight_turn("s-1"));
    }

    #[test]
    fn has_in_flight_turn_true_when_chunks_unterminated() {
        let (_tmp, store) = open_store(1000);
        store
            .record(
                "s-1",
                1,
                &Event::UserPromptSent {
                    text: "go".into(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        store
            .record(
                "s-1",
                2,
                &Event::AgentMessageChunk {
                    text: "thinking".into(),
                },
            )
            .unwrap();
        assert!(store.has_in_flight_turn("s-1"));
    }

    #[test]
    fn has_in_flight_turn_false_when_stopped_after_prompt() {
        let (_tmp, store) = open_store(1000);
        store
            .record(
                "s-1",
                1,
                &Event::UserPromptSent {
                    text: "go".into(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        store
            .record(
                "s-1",
                2,
                &Event::AgentMessageChunk {
                    text: "done".into(),
                },
            )
            .unwrap();
        store
            .record(
                "s-1",
                3,
                &Event::Stopped {
                    reason: "prompt_complete".into(),
                },
            )
            .unwrap();
        assert!(!store.has_in_flight_turn("s-1"));
    }

    #[test]
    fn has_in_flight_turn_false_when_agent_startup_error_after_prompt() {
        let (_tmp, store) = open_store(1000);
        store
            .record(
                "s-1",
                1,
                &Event::UserPromptSent {
                    text: "go".into(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        store
            .record(
                "s-1",
                2,
                &Event::AgentStartupError {
                    message: "boom".into(),
                },
            )
            .unwrap();
        assert!(!store.has_in_flight_turn("s-1"));
    }

    #[test]
    fn has_in_flight_turn_uses_latest_prompt_only() {
        // First turn completed. Second turn in flight. Should return true.
        let (_tmp, store) = open_store(1000);
        store
            .record(
                "s-1",
                1,
                &Event::UserPromptSent {
                    text: "first".into(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        store
            .record(
                "s-1",
                2,
                &Event::Stopped {
                    reason: "prompt_complete".into(),
                },
            )
            .unwrap();
        store
            .record(
                "s-1",
                3,
                &Event::UserPromptSent {
                    text: "second".into(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        store
            .record("s-1", 4, &Event::AgentMessageChunk { text: "mid".into() })
            .unwrap();
        assert!(store.has_in_flight_turn("s-1"));
    }

    #[test]
    fn latest_pending_wakeup_returns_future_wakeup_even_after_user_prompt() {
        // Regression for #1091: the old query treated any UserPromptSent
        // with a higher seq than the WakeupScheduled as evidence the
        // wake had already fired, which hid the sidebar countdown +
        // structured view "Asleep until …" banner whenever the user typed a
        // follow-up message during the wait. Pending now gates purely
        // on `at > now()`.
        let (_tmp, store) = open_store(1000);
        let at = Utc::now() + chrono::Duration::seconds(120);
        store
            .record(
                "s-1",
                1,
                &Event::UserPromptSent {
                    text: "schedule a wake in 2m".into(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        store
            .record(
                "s-1",
                2,
                &Event::WakeupScheduled {
                    at,
                    reason: Some("test wake".into()),
                },
            )
            .unwrap();
        // User-typed follow-up while the wake is still pending.
        store
            .record(
                "s-1",
                3,
                &Event::UserPromptSent {
                    text: "btw, ping me when you wake".into(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        let pending = store.latest_pending_wakeup("s-1").expect("still pending");
        assert!((pending.0 - at).num_seconds().abs() <= 1);
        assert_eq!(pending.1.as_deref(), Some("test wake"));
    }

    #[test]
    fn latest_pending_wakeup_returns_none_when_at_in_past() {
        let (_tmp, store) = open_store(1000);
        let at = Utc::now() - chrono::Duration::seconds(30);
        store
            .record("s-1", 1, &Event::WakeupScheduled { at, reason: None })
            .unwrap();
        assert!(store.latest_pending_wakeup("s-1").is_none());
    }

    #[test]
    fn latest_pending_wakeup_uses_latest_scheduled_event() {
        // When the agent reschedules mid-flight, the latest
        // WakeupScheduled supersedes the earlier one. The query must
        // pick the latest by seq, not by `at` ordering; that's the
        // single source of truth for the active wake.
        let (_tmp, store) = open_store(1000);
        let earlier = Utc::now() + chrono::Duration::seconds(60);
        let later = Utc::now() + chrono::Duration::seconds(600);
        store
            .record(
                "s-1",
                1,
                &Event::WakeupScheduled {
                    at: earlier,
                    reason: Some("first schedule".into()),
                },
            )
            .unwrap();
        store
            .record(
                "s-1",
                2,
                &Event::WakeupScheduled {
                    at: later,
                    reason: Some("rescheduled".into()),
                },
            )
            .unwrap();
        let pending = store.latest_pending_wakeup("s-1").expect("pending");
        assert_eq!(pending.1.as_deref(), Some("rescheduled"));
    }

    #[test]
    fn latest_active_monitor_returns_description_when_armed() {
        let (_tmp, store) = open_store(1000);
        store
            .record(
                "s-1",
                1,
                &Event::MonitorArmed {
                    description: Some("clippy passes".into()),
                },
            )
            .unwrap();
        let armed = store.latest_active_monitor("s-1").expect("armed");
        assert_eq!(armed.as_deref(), Some("clippy passes"));
    }

    #[test]
    fn latest_active_monitor_persists_without_a_user_prompt() {
        // A monitor firing re-invokes the agent with activity but no
        // UserPromptSent, so the badge must persist across those re-fires.
        let (_tmp, store) = open_store(1000);
        store
            .record(
                "s-1",
                1,
                &Event::MonitorArmed {
                    description: Some("build".into()),
                },
            )
            .unwrap();
        // Trailing agent activity (no user prompt) does not clear it.
        store.record("s-1", 2, &Event::ThinkingStarted).unwrap();
        store
            .record(
                "s-1",
                3,
                &Event::AgentMessageChunk {
                    text: "resuming".into(),
                },
            )
            .unwrap();
        assert!(store.latest_active_monitor("s-1").is_some());
    }

    #[test]
    fn latest_active_monitor_clears_on_user_prompt() {
        // The user typing a follow-up means they took over; the badge clears.
        let (_tmp, store) = open_store(1000);
        store
            .record(
                "s-1",
                1,
                &Event::MonitorArmed {
                    description: Some("watch".into()),
                },
            )
            .unwrap();
        store
            .record(
                "s-1",
                2,
                &Event::UserPromptSent {
                    text: "stop watching".into(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        assert!(store.latest_active_monitor("s-1").is_none());
    }

    #[test]
    fn latest_active_monitor_persists_on_arming_turn_stop() {
        // The arming turn ending while the monitor is still pending (no
        // post-arm tool work yet) must NOT clear the badge (#2325).
        let (_tmp, store) = open_store(1000);
        store
            .record(
                "s-1",
                1,
                &Event::MonitorArmed {
                    description: Some("watch".into()),
                },
            )
            .unwrap();
        store
            .record(
                "s-1",
                2,
                &Event::Stopped {
                    reason: "prompt_complete".into(),
                },
            )
            .unwrap();
        assert!(store.latest_active_monitor("s-1").is_some());
    }

    #[test]
    fn latest_active_monitor_clears_after_fired_work_and_stop() {
        // The monitor fired (a tool call started after the arm) and that turn
        // then ended: the badge clears regardless of the Stopped reason, so
        // both the in-band (`prompt_complete`) and between-prompt
        // (`agent_idle`) monitor shapes are covered (#2325).
        for reason in ["prompt_complete", "agent_idle"] {
            let (_tmp, store) = open_store(1000);
            store
                .record(
                    "s-1",
                    1,
                    &Event::MonitorArmed {
                        description: Some("watch".into()),
                    },
                )
                .unwrap();
            store
                .record(
                    "s-1",
                    2,
                    &Event::ToolCallStarted {
                        tool_call: crate::acp::state::ToolCall {
                            id: "tc-1".into(),
                            name: "Read".into(),
                            kind: "read".into(),
                            args_preview: String::new(),
                            started_at: Utc::now(),
                            parent_tool_call_id: None,
                            memory_recall: None,
                            diffs: Vec::new(),
                        },
                    },
                )
                .unwrap();
            // Tool started but turn not yet ended: badge still up.
            assert!(store.latest_active_monitor("s-1").is_some());
            store
                .record(
                    "s-1",
                    3,
                    &Event::Stopped {
                        reason: reason.into(),
                    },
                )
                .unwrap();
            assert!(
                store.latest_active_monitor("s-1").is_none(),
                "badge should clear after fired work + Stopped({reason})"
            );
        }
    }

    #[test]
    fn latest_active_monitor_none_without_monitor() {
        let (_tmp, store) = open_store(1000);
        store
            .record(
                "s-1",
                1,
                &Event::UserPromptSent {
                    text: "hi".into(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        assert!(store.latest_active_monitor("s-1").is_none());
    }

    #[test]
    fn fired_wakeup_for_prompt_skips_mid_wait_user_followup() {
        // Regression for #1091: a user-typed prompt arriving BEFORE the
        // wake `at` must not count as the wake firing. Same flaw as
        // `latest_pending_wakeup`; mirrored here so we don't dispatch
        // a false-positive push notification.
        let (_tmp, store) = open_store(1000);
        // Wake `at` is in the future relative to the follow-up prompt
        // we'll record. Use a 5-minute offset so the test isn't racy
        // against wall-clock skew.
        let at = Utc::now() + chrono::Duration::seconds(300);
        store
            .record(
                "s-1",
                1,
                &Event::UserPromptSent {
                    text: "schedule a wake".into(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        store
            .record(
                "s-1",
                2,
                &Event::WakeupScheduled {
                    at,
                    reason: Some("test wake".into()),
                },
            )
            .unwrap();
        // Mid-wait follow-up: created now, but the wake `at` is +5m.
        store
            .record(
                "s-1",
                3,
                &Event::UserPromptSent {
                    text: "ping me when you wake".into(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        assert!(
            store.fired_wakeup_for_prompt("s-1", 3).is_none(),
            "mid-wait follow-up must not count as wake-fire",
        );
    }

    #[test]
    fn fired_wakeup_for_prompt_returns_first_prompt_past_wake_at() {
        let (_tmp, store) = open_store(1000);
        let at = Utc::now() - chrono::Duration::seconds(5);
        store
            .record(
                "s-1",
                1,
                &Event::WakeupScheduled {
                    at,
                    reason: Some("test wake".into()),
                },
            )
            .unwrap();
        store
            .record(
                "s-1",
                2,
                &Event::UserPromptSent {
                    text: "Wake-up fired. Confirm.".into(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        let fired = store
            .fired_wakeup_for_prompt("s-1", 2)
            .expect("first prompt past wake `at` is the wake-fire");
        assert_eq!(fired.1.as_deref(), Some("test wake"));
    }

    #[test]
    fn fired_wakeup_for_prompt_doesnt_double_claim() {
        // Once a prompt has claimed the wake-fire, later prompts on
        // the same wake must not re-fire the push.
        let (_tmp, store) = open_store(1000);
        let at = Utc::now() - chrono::Duration::seconds(60);
        store
            .record("s-1", 1, &Event::WakeupScheduled { at, reason: None })
            .unwrap();
        store
            .record(
                "s-1",
                2,
                &Event::UserPromptSent {
                    text: "first prompt past at".into(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        store
            .record(
                "s-1",
                3,
                &Event::UserPromptSent {
                    text: "second prompt past at".into(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        assert!(store.fired_wakeup_for_prompt("s-1", 2).is_some());
        assert!(
            store.fired_wakeup_for_prompt("s-1", 3).is_none(),
            "second prompt past the wake's `at` must not claim again",
        );
    }

    #[test]
    fn store_persists_across_reopen() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("acp.db");
        {
            let store = EventStore::open(&path, 1000).unwrap();
            store
                .record(
                    "s-1",
                    1,
                    &Event::UserPromptSent {
                        text: "hello".into(),
                        attachments: Vec::new(),
                    },
                )
                .unwrap();
            store
                .record(
                    "s-1",
                    2,
                    &Event::AgentMessageChunk {
                        text: "hi back".into(),
                    },
                )
                .unwrap();
        }
        // Drop and reopen the store; the rows should still be there.
        let store = EventStore::open(&path, 1000).unwrap();
        let replay = store.replay_from("s-1", 0);
        assert_eq!(replay.len(), 2);
        assert_eq!(store.highest_seq("s-1"), 2);
    }

    /// `latest_status_event` returns the most recent lifecycle event the
    /// sidebar status derivation cares about. Used by the startup
    /// seeding pass (#1103) so a session that was mid-turn when the
    /// previous daemon died renders Running on cold start.
    #[test]
    fn latest_status_event_returns_most_recent_lifecycle_event() {
        let (_tmp, store) = open_store(1000);
        store
            .record(
                "s-1",
                1,
                &Event::UserPromptSent {
                    text: "hi".into(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        store.record("s-1", 2, &Event::ThinkingStarted).unwrap();
        store.record("s-1", 3, &Event::ThinkingEnded).unwrap();
        // Most recent matching event is the UserPromptSent at seq 1.
        let latest = store.latest_status_event("s-1");
        assert!(matches!(
            latest,
            Some(Event::UserPromptSent { text, .. }) if text == "hi"
        ));

        // Stopped at seq 4 takes over as the most recent lifecycle event.
        store
            .record(
                "s-1",
                4,
                &Event::Stopped {
                    reason: "prompt_complete".into(),
                },
            )
            .unwrap();
        let latest = store.latest_status_event("s-1");
        assert!(matches!(latest, Some(Event::Stopped { reason }) if reason == "prompt_complete"));

        // Session with no lifecycle events → None.
        store.record("s-2", 1, &Event::ThinkingStarted).unwrap();
        assert!(store.latest_status_event("s-2").is_none());

        // Unknown session → None.
        assert!(store.latest_status_event("nope").is_none());
    }

    /// `latest_seed_status_event` also matches agent-transcript activity
    /// events, so a turn the agent resumed on its own after a terminal
    /// `Stopped{prompt_complete}` (no new `UserPromptSent`) seeds Running,
    /// not a stale Idle, across a daemon restart. `latest_status_event`
    /// keeps ignoring those activity events so rate-limit park detection is
    /// unaffected. See #2625.
    #[test]
    fn latest_seed_status_event_sees_activity_after_stopped() {
        let (_tmp, store) = open_store(1000);
        store
            .record(
                "s-1",
                1,
                &Event::UserPromptSent {
                    text: "go".into(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        store
            .record(
                "s-1",
                2,
                &Event::Stopped {
                    reason: "prompt_complete".into(),
                },
            )
            .unwrap();
        // Agent resumes on its own: streams more output with no new prompt.
        store
            .record(
                "s-1",
                3,
                &Event::AgentMessageChunk {
                    text: "build done".into(),
                },
            )
            .unwrap();

        // Seed derivation follows the activity: the newest status-relevant
        // event is the AgentMessageChunk, which the live path maps to Running.
        assert!(matches!(
            store.latest_seed_status_event("s-1"),
            Some(Event::AgentMessageChunk { text }) if text == "build done"
        ));
        // Park detection still sees only the terminal Stopped.
        assert!(matches!(
            store.latest_status_event("s-1"),
            Some(Event::Stopped { reason }) if reason == "prompt_complete"
        ));

        // A `ThinkingStarted` alone (no other lifecycle event) seeds via the
        // activity set, whereas the narrow query returns None.
        store.record("s-2", 1, &Event::ThinkingStarted).unwrap();
        assert!(matches!(
            store.latest_seed_status_event("s-2"),
            Some(Event::ThinkingStarted)
        ));
        assert!(store.latest_status_event("s-2").is_none());
    }

    /// A rate-limit park must not be hidden from `latest_status_event` by a
    /// trailing activity event, or the auto-resume logic would try to resume
    /// a quota-blocked session. `latest_seed_status_event` is free to move on
    /// (the sidebar shows the resumed work), but the park query stays put.
    /// See #1722, #2625.
    #[test]
    fn latest_status_event_ignores_activity_after_rate_limit_park() {
        let (_tmp, store) = open_store(1000);
        store
            .record(
                "s-1",
                1,
                &Event::Stopped {
                    reason: "rate_limited".into(),
                },
            )
            .unwrap();
        store
            .record(
                "s-1",
                2,
                &Event::AgentMessageChunk {
                    text: "trailing".into(),
                },
            )
            .unwrap();

        assert!(matches!(
            store.latest_status_event("s-1"),
            Some(Event::Stopped { reason }) if reason == "rate_limited"
        ));
        assert!(matches!(
            store.latest_seed_status_event("s-1"),
            Some(Event::AgentMessageChunk { text }) if text == "trailing"
        ));
    }

    /// `unresolved_approval_nonces` finds `ApprovalRequested` rows whose
    /// nonce never saw a matching `ApprovalResolved`. Used by
    /// `Supervisor::attach` to clear approval cards orphaned by daemon
    /// restart (#1099).
    #[test]
    fn unresolved_approval_nonces_finds_orphaned_requests() {
        use crate::acp::approvals::{Approval, ApprovalDecision, Nonce};
        use crate::acp::state::ToolCall;

        let (_tmp, store) = open_store(1000);
        let tool_call = ToolCall {
            id: "tc-1".into(),
            name: "Bash".into(),
            kind: "execute".into(),
            args_preview: "ls".into(),
            started_at: Utc::now(),
            parent_tool_call_id: None,
            memory_recall: None,
            diffs: Vec::new(),
        };
        let nonce_a = Nonce("aaaa".into());
        let nonce_b = Nonce("bbbb".into());
        let nonce_c = Nonce("cccc".into());
        let approval_a = Approval {
            nonce: nonce_a.clone(),
            tool_call: tool_call.clone(),
            destructive: false,
            requested_at: Utc::now(),
            resolved: None,
        };
        let approval_b = Approval {
            nonce: nonce_b.clone(),
            tool_call: tool_call.clone(),
            destructive: false,
            requested_at: Utc::now(),
            resolved: None,
        };
        let approval_c = Approval {
            nonce: nonce_c.clone(),
            tool_call,
            destructive: false,
            requested_at: Utc::now(),
            resolved: None,
        };
        // A is requested and resolved. B and C are requested but never
        // resolved (orphans).
        store
            .record(
                "s-1",
                1,
                &Event::ApprovalRequested {
                    approval: approval_a,
                },
            )
            .unwrap();
        store
            .record(
                "s-1",
                2,
                &Event::ApprovalResolved {
                    nonce: nonce_a,
                    decision: ApprovalDecision::Allow,
                },
            )
            .unwrap();
        store
            .record(
                "s-1",
                3,
                &Event::ApprovalRequested {
                    approval: approval_b,
                },
            )
            .unwrap();
        store
            .record(
                "s-1",
                4,
                &Event::ApprovalRequested {
                    approval: approval_c,
                },
            )
            .unwrap();

        let mut orphans = store.unresolved_approval_nonces("s-1");
        orphans.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(orphans, vec![nonce_b, nonce_c]);

        // Unrelated session must not bleed into the query.
        assert!(store.unresolved_approval_nonces("s-2").is_empty());
    }

    fn orphan_test_elicitation(nonce: &Nonce) -> crate::acp::elicitations::Elicitation {
        crate::acp::elicitations::Elicitation {
            nonce: nonce.clone(),
            message: "Pick".into(),
            title: None,
            description: None,
            tool_call_id: None,
            questions: Vec::new(),
            requested_at: Utc::now(),
            resolved: None,
        }
    }

    /// Elicitation parallel of `unresolved_approval_nonces`: an
    /// `ElicitationRequested` whose nonce never saw a matching
    /// `ElicitationResolved` is reported as orphaned on reattach.
    #[test]
    fn unresolved_elicitation_nonces_finds_orphaned_requests() {
        use crate::acp::approvals::Nonce;
        use crate::acp::elicitations::ElicitationOutcome;

        let (_tmp, store) = open_store(1000);
        let nonce_a = Nonce::new();
        let nonce_b = Nonce::new();
        store
            .record(
                "s-1",
                1,
                &Event::ElicitationRequested {
                    elicitation: orphan_test_elicitation(&nonce_a),
                },
            )
            .unwrap();
        store
            .record(
                "s-1",
                2,
                &Event::ElicitationRequested {
                    elicitation: orphan_test_elicitation(&nonce_b),
                },
            )
            .unwrap();
        // Only nonce_a is resolved; nonce_b stays orphaned.
        store
            .record(
                "s-1",
                3,
                &Event::ElicitationResolved {
                    nonce: nonce_a,
                    outcome: ElicitationOutcome::Accepted,
                    answers: Vec::new(),
                },
            )
            .unwrap();

        assert_eq!(store.unresolved_elicitation_nonces("s-1"), vec![nonce_b]);
        // Unrelated session must not bleed into the query.
        assert!(store.unresolved_elicitation_nonces("s-2").is_empty());
    }

    /// `latest_status_event` must recognize elicitation lifecycle events,
    /// so a session blocked on a pending elicitation re-derives to Waiting
    /// on cold-start / attach instead of waiting for the next live event.
    #[test]
    fn latest_status_event_includes_elicitation_lifecycle() {
        use crate::acp::approvals::Nonce;
        use crate::acp::elicitations::ElicitationOutcome;

        let (_tmp, store) = open_store(1000);
        let nonce_a = Nonce::new();
        store
            .record(
                "s-1",
                1,
                &Event::ElicitationRequested {
                    elicitation: orphan_test_elicitation(&nonce_a),
                },
            )
            .unwrap();
        assert!(matches!(
            store.latest_status_event("s-1"),
            Some(Event::ElicitationRequested { .. })
        ));

        store
            .record(
                "s-1",
                2,
                &Event::ElicitationResolved {
                    nonce: nonce_a,
                    outcome: ElicitationOutcome::Accepted,
                    answers: Vec::new(),
                },
            )
            .unwrap();
        assert!(matches!(
            store.latest_status_event("s-1"),
            Some(Event::ElicitationResolved { .. })
        ));
    }

    fn rate_limit_event(secs_until_reset: i64) -> Event {
        Event::RateLimit {
            info: RateLimitInfo {
                status: "usage limit reached".into(),
                resets_at: Utc::now() + chrono::Duration::seconds(secs_until_reset),
                kind: "rate_limit".into(),
            },
        }
    }

    #[test]
    fn latest_rate_limit_event_returns_most_recent_with_recorded_at() {
        let (_tmp, store) = open_store(1000);
        let before = Utc::now().timestamp_millis();
        store.record("s-1", 1, &rate_limit_event(3600)).unwrap();
        // A second, later rate limit supersedes the first (new resets_at).
        let second = rate_limit_event(7200);
        let Event::RateLimit { info: ref expected } = second else {
            unreachable!()
        };
        let expected_resets = expected.resets_at;
        store.record("s-1", 2, &second).unwrap();
        let after = Utc::now().timestamp_millis();

        let (info, recorded_at) = store
            .latest_rate_limit_event("s-1")
            .expect("a rate-limit event is stored");
        assert_eq!(info.resets_at, expected_resets, "latest event wins");
        assert!(
            recorded_at >= before && recorded_at <= after,
            "recorded_at ({recorded_at}) is the row's created_at within [{before}, {after}]"
        );
        // A session with no rate-limit event returns None.
        assert!(store.latest_rate_limit_event("s-2").is_none());
    }

    #[test]
    fn rate_limit_auto_resumed_supersedes_stopped_in_latest_status() {
        let (_tmp, store) = open_store(1000);
        store.record("s-1", 1, &rate_limit_event(60)).unwrap();
        store
            .record(
                "s-1",
                2,
                &Event::Stopped {
                    reason: "rate_limited".into(),
                },
            )
            .unwrap();
        // While parked, the latest status is the rate-limit Stopped.
        assert!(matches!(
            store.latest_status_event("s-1"),
            Some(Event::Stopped { reason }) if reason == "rate_limited"
        ));
        // The auto-resume breadcrumb must become the latest status event so
        // the reconciler's resume loop stops seeing the park. See #1722.
        let resets_at = Utc::now();
        store
            .record("s-1", 3, &Event::RateLimitAutoResumed { resets_at })
            .unwrap();
        assert!(
            matches!(
                store.latest_status_event("s-1"),
                Some(Event::RateLimitAutoResumed { .. })
            ),
            "RateLimitAutoResumed must supersede Stopped{{rate_limited}}"
        );
    }

    #[test]
    fn replay_page_before_returns_closest_below_in_asc_order() {
        let (_tmp, store) = open_store(1000);
        for seq in 1..=10 {
            store.record("s-1", seq, &Event::ThinkingStarted).unwrap();
        }
        // Tail: newest page is requested with before = u64::MAX.
        let tail = store.replay_page_before("s-1", u64::MAX, Some(3));
        let seqs: Vec<u64> = tail.events.iter().map(|(seq, _)| *seq).collect();
        // The CLOSEST 3 below the cursor, ascending (not the oldest 3).
        assert_eq!(seqs, vec![8, 9, 10]);
        assert!(tail.has_more, "older events remain below seq 8");
        assert_eq!(
            tail.last_scanned_seq,
            Some(8),
            "cursor is the page's lowest"
        );
        assert_eq!(tail.highest_seq, 10);

        // Next-older page pages back via before = previous lowest.
        let older = store.replay_page_before("s-1", 8, Some(3));
        let seqs: Vec<u64> = older.events.iter().map(|(seq, _)| *seq).collect();
        assert_eq!(seqs, vec![5, 6, 7]);
        assert!(older.has_more);
        assert_eq!(older.last_scanned_seq, Some(5));
    }

    #[test]
    fn replay_page_before_stops_at_session_start() {
        let (_tmp, store) = open_store(1000);
        for seq in 1..=4 {
            store.record("s-1", seq, &Event::ThinkingStarted).unwrap();
        }
        // A page that reaches the very first event clears has_more so the
        // client knows not to keep paging older.
        let page = store.replay_page_before("s-1", 3, Some(10));
        let seqs: Vec<u64> = page.events.iter().map(|(seq, _)| *seq).collect();
        assert_eq!(seqs, vec![1, 2]);
        assert!(!page.has_more, "reached session start, nothing older");
        // Empty when before predates everything stored.
        let none = store.replay_page_before("s-1", 1, Some(10));
        assert!(none.events.is_empty());
        assert!(!none.has_more);
    }

    #[test]
    fn replay_page_before_trims_leading_partial_turn_to_boundary() {
        let (_tmp, store) = open_store(1000);
        // seq 1: handshake-ish, 2: prompt A, 3-4: A's turn, 5: prompt B,
        // 6-7: B's turn.
        store
            .record(
                "s-1",
                1,
                &Event::PromptCapabilities {
                    image: true,
                    audio: false,
                    embedded_context: true,
                },
            )
            .unwrap();
        let prompt = |t: &str| Event::UserPromptSent {
            text: t.into(),
            attachments: vec![],
        };
        store.record("s-1", 2, &prompt("A")).unwrap();
        store.record("s-1", 3, &Event::ThinkingStarted).unwrap();
        store.record("s-1", 4, &Event::ThinkingStarted).unwrap();
        store.record("s-1", 5, &prompt("B")).unwrap();
        store.record("s-1", 6, &Event::ThinkingStarted).unwrap();
        store.record("s-1", 7, &Event::ThinkingStarted).unwrap();

        // Tail of 4 frames would be seq 4..7, but seq 4 is mid-turn A.
        // With has_more, the page is trimmed to start at prompt B (seq 5).
        let tail = store.replay_page_before("s-1", u64::MAX, Some(4));
        let seqs: Vec<u64> = tail.events.iter().map(|(seq, _)| *seq).collect();
        assert_eq!(seqs, vec![5, 6, 7], "leading partial turn A trimmed");
        assert!(tail.has_more);
        assert_eq!(tail.last_scanned_seq, Some(5), "cursor is boundary B");

        // Next page (before=5) reaches session start: keep everything,
        // including the seq-1 handshake, so a one-page load isn't stripped.
        let older = store.replay_page_before("s-1", 5, Some(10));
        let seqs: Vec<u64> = older.events.iter().map(|(seq, _)| *seq).collect();
        assert_eq!(seqs, vec![1, 2, 3, 4]);
        assert!(!older.has_more);
    }
}
