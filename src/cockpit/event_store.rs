//! Disk-backed event log for cockpit sessions.
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
//!   `/sessions/{id}/cockpit/ws`. The handler reads
//!   `replay_from(session_id, since)` out of this store and pushes
//!   those frames before forwarding the live broadcast, closing the
//!   subscribe-gap race that would otherwise drop the agent's first
//!   chunks on a fast page load.
//! - **Snapshot endpoint.** `GET /cockpit/replay?since=N` reads the
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
//! `Instance.cockpit_acp_session_id` and uses `session/load` on
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
//! `cockpit_disable` (the master switch turning off, or a per-session
//! opt-out). The connection has WAL mode enabled so the publish path
//! and the replay endpoint don't block each other under load.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use tracing::{debug, trace, warn};

use super::approvals::Nonce;
use super::state::{Event, Plan};

/// Externally-tagged JSON discriminants for non-substantive cockpit
/// events: lifecycle and metadata snapshots the agent emits once per
/// session (or on every cold-start resume) that must not count as session
/// activity. Two SQL predicates depend on this set staying in sync: the
/// retention prune exempts them from eviction (#1049) and the idle-reap
/// idle clock ignores them (#1689). Centralized so the two cannot silently
/// desync.
const NON_SUBSTANTIVE_EVENT_DISCRIMINANTS: &[&str] = &[
    "AvailableCommandsUpdated",
    "ModesAvailable",
    "CurrentModeChanged",
    "AcpSessionAssigned",
    "PromptCapabilities",
];

/// Build the `AND event_json NOT LIKE '{"Name":%'` SQL fragment for every
/// non-substantive discriminant, newline-joined for readable queries.
fn non_substantive_not_like_clauses() -> String {
    NON_SUBSTANTIVE_EVENT_DISCRIMINANTS
        .iter()
        .map(|name| format!("AND event_json NOT LIKE '{{\"{name}\":%'"))
        .collect::<Vec<_>>()
        .join("\n               ")
}

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

/// Highest seq stored for `session_id`, or 0 if none. Free fn so both
/// the public [`EventStore::highest_seq`] and the single-snapshot
/// [`EventStore::replay_page`] can share it under one held lock.
fn query_highest_seq(conn: &Connection, session_id: &str) -> u64 {
    match conn
        .query_row(
            "SELECT MAX(seq) FROM cockpit_events WHERE session_id = ?1",
            params![session_id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()
    {
        Ok(Some(Some(max))) => max as u64,
        _ => 0,
    }
}

/// Lowest seq still stored for `session_id`, or `None` when empty.
/// Companion to [`query_highest_seq`].
fn query_lowest_seq(conn: &Connection, session_id: &str) -> Option<u64> {
    match conn
        .query_row(
            "SELECT MIN(seq) FROM cockpit_events WHERE session_id = ?1",
            params![session_id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()
    {
        Ok(Some(Some(m))) => Some(m as u64),
        _ => None,
    }
}

/// SQLite-backed cockpit event log. One row per (session_id, seq).
pub struct EventStore {
    conn: Mutex<Connection>,
    /// Per-session retention cap. Older events are pruned on insert
    /// once the count exceeds this value. Bytes are not enforced here
    /// (the in-memory ring still has a byte cap); the row count keeps
    /// the on-disk size bounded.
    max_events_per_session: usize,
}

impl EventStore {
    /// Open or create the database at `db_path`. Creates the
    /// `cockpit_events` table if missing. The connection has WAL mode
    /// enabled so concurrent writers (publish path) and readers
    /// (replay endpoint) don't block each other.
    pub fn open(db_path: &Path, max_events_per_session: usize) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("create parent dir for cockpit DB at {}", parent.display())
                })?;
            }
        }
        let conn = Connection::open(db_path)
            .with_context(|| format!("open cockpit DB at {}", db_path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("enable WAL mode")?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .context("set synchronous=NORMAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS cockpit_events (
                session_id  TEXT    NOT NULL,
                seq         INTEGER NOT NULL,
                event_json  TEXT    NOT NULL,
                created_at  INTEGER NOT NULL,
                PRIMARY KEY (session_id, seq)
            );
            CREATE INDEX IF NOT EXISTS idx_cockpit_events_session_seq
                ON cockpit_events(session_id, seq);
            CREATE INDEX IF NOT EXISTS idx_cockpit_events_session_created_at
                ON cockpit_events(session_id, created_at);
            CREATE TABLE IF NOT EXISTS cockpit_attachments (
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
            CREATE INDEX IF NOT EXISTS idx_cockpit_attachments_session_seq
                ON cockpit_attachments(session_id, seq);",
        )
        .context("create cockpit_events schema")?;
        debug!(
            target: "cockpit.event_store",
            path = %db_path.display(),
            cap = max_events_per_session,
            "cockpit event store opened"
        );
        Ok(Self {
            conn: Mutex::new(conn),
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
        let inserted = conn
            .execute(
                "INSERT OR IGNORE INTO cockpit_events (session_id, seq, event_json, created_at)
             VALUES (?1, ?2, ?3, ?4)",
                params![session_id, seq as i64, json, now_ms],
            )
            .with_context(|| format!("insert {session_id}@{seq}"))?;
        if inserted == 0 {
            // Primary-key collision: same (session_id, seq) seen before.
            // Logged at trace because the cause is usually a benign retry
            // (publish_user_prompt + replay drain re-publishing) rather
            // than a bug, but we still want a breadcrumb. Per-event lines
            // are too noisy to live at debug; they bury the lifecycle
            // signal in debug.log during an active turn.
            trace!(
                target: "cockpit.event_store",
                session = %session_id,
                seq,
                kind,
                "skipped duplicate event (already on disk)"
            );
        } else {
            trace!(
                target: "cockpit.event_store",
                session = %session_id,
                seq,
                kind,
                bytes,
                "recorded event"
            );
        }
        // Prune oldest beyond the retention cap. Cheap when below the cap
        // (the subquery returns 0 rows). We do it on every insert rather
        // than periodically so the upper bound on per-session disk usage
        // is strict rather than amortised.
        //
        // Snapshot events (the slash-command list, mode list, ACP session
        // id) are exempt from pruning: the agent only emits them once per
        // session lifecycle, near the start of the seq range, so a long
        // session blows past the cap and evicts them; leaving the
        // composer's `/` palette and the mode picker empty on reconnect.
        // See #1049. The `event_json NOT LIKE` clauses match the
        // externally-tagged JSON discriminant for each pinned variant.
        if self.max_events_per_session > 0 {
            // Compute the prune cutoff seq once so the events delete and
            // the attachments delete agree on the same threshold. Doing
            // the events delete first would shift the OFFSET row out from
            // under the attachments delete and leave orphaned blobs.
            let cutoff: Option<i64> = conn
                .query_row(
                    "SELECT seq FROM cockpit_events
                     WHERE session_id = ?1
                     ORDER BY seq DESC
                     LIMIT 1 OFFSET ?2",
                    params![session_id, self.max_events_per_session as i64],
                    |row| row.get(0),
                )
                .optional()
                .unwrap_or(None);
            if let Some(cutoff) = cutoff {
                // The `non_substantive_not_like_clauses()` helper pins the
                // snapshot variants (slash-command list, mode list, ACP
                // session id) so a long session can't evict them. See #1049.
                let prune_sql = format!(
                    "DELETE FROM cockpit_events
                     WHERE session_id = ?1
                       AND seq <= ?2
                       {clauses}",
                    clauses = non_substantive_not_like_clauses()
                );
                match conn.execute(&prune_sql, params![session_id, cutoff]) {
                    Ok(0) => {}
                    Ok(pruned) => {
                        trace!(
                            target: "cockpit.event_store",
                            session = %session_id,
                            pruned,
                            cap = self.max_events_per_session,
                            "pruned oldest events past retention cap"
                        );
                    }
                    Err(e) => {
                        // Prune failure isn't fatal; the row is recorded,
                        // we just exceed the cap until the next prune
                        // succeeds. Log + swallow so callers don't have to
                        // distinguish "record failed" from "trim failed".
                        warn!(target: "cockpit.event_store", "prune {session_id}: {e}");
                    }
                }
                // Drop attachment blobs whose owning UserPromptSent has
                // (or is about to be) pruned. Attachments only ever hang
                // off prompts, never the pinned snapshot variants, so a
                // flat `seq <= cutoff` delete cannot strand a blob whose
                // event survived. This is what keeps the attachment table
                // bounded alongside the event log rather than growing
                // without limit as screenshots accumulate.
                if let Err(e) = conn.execute(
                    "DELETE FROM cockpit_attachments
                     WHERE session_id = ?1 AND seq <= ?2",
                    params![session_id, cutoff],
                ) {
                    warn!(target: "cockpit.event_store", "prune attachments {session_id}: {e}");
                }
            }
        }
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
        let mut stmt = match conn.prepare(
            "SELECT seq, event_json FROM cockpit_events
             WHERE session_id = ?1 AND seq < ?2
             ORDER BY seq ASC",
        ) {
            Ok(s) => s,
            Err(e) => {
                warn!(target: "cockpit.event_store", "prepare replay_before for {session_id}: {e}");
                return Vec::new();
            }
        };
        let rows = match stmt.query_map(params![session_id, before as i64], |row| {
            let seq: i64 = row.get(0)?;
            let json: String = row.get(1)?;
            Ok((seq as u64, json))
        }) {
            Ok(r) => r,
            Err(e) => {
                warn!(target: "cockpit.event_store", "query replay_before for {session_id}: {e}");
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        for row in rows {
            match row {
                Ok((seq, json)) => match serde_json::from_str::<Event>(&json) {
                    Ok(event) => out.push((seq, event)),
                    Err(e) => warn!(
                        target: "cockpit.event_store",
                        "deserialise event {session_id}@{seq}: {e}"
                    ),
                },
                Err(e) => warn!(target: "cockpit.event_store", "row error: {e}"),
            }
        }
        out
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
        let highest_seq = query_highest_seq(&conn, session_id);
        let lowest_seq = query_lowest_seq(&conn, session_id);
        // `seq` is a signed SQLite column; clamp before the cast so the
        // status probe's `since = u64::MAX` doesn't wrap to -1 and match
        // every row (`seq > -1`) instead of returning an empty page.
        let since_i64 = i64::try_from(since).unwrap_or(i64::MAX);
        // Probe one extra row beyond `limit` to detect `has_more` without
        // a second query against `highest_seq`.
        let probe = limit.map(|n| n.saturating_add(1));
        let sql = match probe {
            Some(_) => {
                "SELECT seq, event_json FROM cockpit_events
                 WHERE session_id = ?1 AND seq > ?2
                 ORDER BY seq ASC LIMIT ?3"
            }
            None => {
                "SELECT seq, event_json FROM cockpit_events
                 WHERE session_id = ?1 AND seq > ?2
                 ORDER BY seq ASC"
            }
        };
        let mut stmt = match conn.prepare(sql) {
            Ok(s) => s,
            Err(e) => {
                warn!(target: "cockpit.event_store", "prepare replay for {session_id}: {e}");
                return ReplayPage {
                    events: Vec::new(),
                    last_scanned_seq: None,
                    has_more: false,
                    highest_seq,
                    lowest_seq,
                };
            }
        };
        let map_row = |row: &rusqlite::Row| {
            let seq: i64 = row.get(0)?;
            let json: String = row.get(1)?;
            Ok((seq as u64, json))
        };
        let rows = match probe {
            Some(p) => stmt.query_map(params![session_id, since_i64, p as i64], map_row),
            None => stmt.query_map(params![session_id, since_i64], map_row),
        };
        let rows = match rows {
            Ok(r) => r,
            Err(e) => {
                warn!(target: "cockpit.event_store", "query replay for {session_id}: {e}");
                return ReplayPage {
                    events: Vec::new(),
                    last_scanned_seq: None,
                    has_more: false,
                    highest_seq,
                    lowest_seq,
                };
            }
        };
        let mut out = Vec::new();
        let mut last_scanned_seq = None;
        let mut scanned = 0usize;
        let mut has_more = false;
        for row in rows {
            match row {
                Ok((seq, json)) => {
                    // The probe row proves another page exists; don't
                    // consume it or advance the cursor onto it.
                    if let Some(n) = limit {
                        if scanned == n {
                            has_more = true;
                            break;
                        }
                    }
                    scanned += 1;
                    last_scanned_seq = Some(seq);
                    match serde_json::from_str::<Event>(&json) {
                        Ok(event) => out.push((seq, event)),
                        Err(e) => warn!(
                            target: "cockpit.event_store",
                            "deserialise event {session_id}@{seq}: {e}"
                        ),
                    }
                }
                Err(e) => warn!(target: "cockpit.event_store", "row error: {e}"),
            }
        }
        trace!(
            target: "cockpit.event_store",
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

    /// Return the latest `Event::PlanUpdated` stored for `session_id`,
    /// if any. Used by the REST sessions endpoint to surface
    /// plan-progress chrome (current step / completed / total) on the
    /// sidebar without subscribing to the cockpit WS for every session.
    /// See #1061.
    pub fn latest_plan(&self, session_id: &str) -> Option<Plan> {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let json: String = conn
            .query_row(
                "SELECT event_json FROM cockpit_events
                 WHERE session_id = ?1
                   AND event_json LIKE '{\"PlanUpdated\":%'
                 ORDER BY seq DESC LIMIT 1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten()?;
        let event: Event = serde_json::from_str(&json).ok()?;
        if let Event::PlanUpdated { plan } = event {
            Some(plan)
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
        let json: Option<String> = conn
            .query_row(
                "SELECT event_json FROM cockpit_events
                 WHERE session_id = ?1
                   AND event_json LIKE '{\"WakeupScheduled\":%'
                 ORDER BY seq DESC LIMIT 1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten();
        // No log for the "no row" branch. The web UI polls /api/sessions
        // every ~2-3s and fans this query out per cockpit session; every
        // idle session would land here on every poll. The "still pending"
        // and "treated as fired" branches below stay at trace because
        // those carry the wake `at` timestamp, which is the only
        // diagnostic value of this function.
        let json = json?;
        let event: Event = match serde_json::from_str(&json) {
            Ok(e) => e,
            Err(e) => {
                warn!(
                    target: "cockpit.event_store",
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
                    target: "cockpit.event_store",
                    session = %session_id,
                    wake_at = %at,
                    in_secs = (at - now).num_seconds(),
                    "latest_pending_wakeup: still pending"
                );
                Some((at, reason))
            } else {
                trace!(
                    target: "cockpit.event_store",
                    session = %session_id,
                    wake_at = %at,
                    elapsed_secs = (now - at).num_seconds(),
                    "latest_pending_wakeup: wake `at` in past; treating as fired"
                );
                None
            }
        } else {
            None
        }
    }

    /// Given the seq of a just-published `UserPromptSent`, return the
    /// `WakeupScheduled` whose timer just fired (so the cockpit event
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
                "SELECT created_at FROM cockpit_events
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
                    target: "cockpit.event_store",
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
                "SELECT seq, event_json FROM cockpit_events
                 WHERE session_id = ?1
                   AND seq < ?2
                   AND event_json LIKE '{\"WakeupScheduled\":%'
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
                    target: "cockpit.event_store",
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
                    target: "cockpit.event_store",
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
                target: "cockpit.event_store",
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
                "SELECT COUNT(*) FROM cockpit_events
                 WHERE session_id = ?1
                   AND seq > ?2
                   AND seq < ?3
                   AND event_json LIKE '{\"UserPromptSent\":%'
                   AND created_at >= ?4",
                params![session_id, wake_seq, prompt_seq_i64, at_ms],
                |row| row.get(0),
            )
            .unwrap_or(0);
        if claimed > 0 {
            debug!(
                target: "cockpit.event_store",
                session = %session_id,
                prompt_seq,
                wake_seq,
                claimed,
                "fired_wakeup_for_prompt: another prompt already claimed this wake"
            );
            return None;
        }
        debug!(
            target: "cockpit.event_store",
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
        let max = query_highest_seq(&conn, session_id);
        trace!(
            target: "cockpit.event_store",
            session = %session_id,
            highest_seq = max,
            "highest_seq query"
        );
        max
    }

    /// Return the lowest seq still stored for `session_id`, or `None`
    /// if the session has no events on disk (either never wrote any, or
    /// the retention cap has evicted them all). Used by `/cockpit/replay`
    /// to compute whether a client's `since` cursor falls below the
    /// pruned floor so the response can signal `lost = true`.
    pub fn lowest_seq(&self, session_id: &str) -> Option<u64> {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let min = query_lowest_seq(&conn, session_id);
        trace!(
            target: "cockpit.event_store",
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
        let mut stmt = match conn
            .prepare("SELECT session_id, MAX(seq) FROM cockpit_events GROUP BY session_id")
        {
            Ok(s) => s,
            Err(e) => {
                warn!(target: "cockpit.event_store", "prepare all_session_seqs: {e}");
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
                warn!(target: "cockpit.event_store", "query all_session_seqs: {e}");
                return Vec::new();
            }
        };
        let collected: Vec<(String, u64)> = rows.filter_map(|r| r.ok()).collect();
        debug!(
            target: "cockpit.event_store",
            sessions = collected.len(),
            "all_session_seqs hydration"
        );
        collected
    }

    /// Latest event for `session_id` that the sidebar status derivation
    /// cares about. Used at daemon startup to seed `Instance.status`
    /// from history: the in-memory status writes that fire on live
    /// cockpit events don't survive restart, so without this scan a
    /// session that was mid-turn when the previous daemon died would
    /// render Idle until the next lifecycle event arrived. See #1103.
    pub fn latest_status_event(&self, session_id: &str) -> Option<Event> {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let json: Option<String> = conn
            .query_row(
                "SELECT event_json FROM cockpit_events
                 WHERE session_id = ?1
                   AND (json_extract(event_json, '$.UserPromptSent') IS NOT NULL
                     OR json_extract(event_json, '$.ApprovalRequested') IS NOT NULL
                     OR json_extract(event_json, '$.ApprovalResolved') IS NOT NULL
                     OR json_extract(event_json, '$.Stopped') IS NOT NULL
                     OR json_extract(event_json, '$.AgentStartupError') IS NOT NULL)
                 ORDER BY seq DESC
                 LIMIT 1",
                params![session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .unwrap_or_else(|e| {
                warn!(
                    target: "cockpit.event_store",
                    "latest_status_event query for {session_id}: {e}"
                );
                None
            });
        json.and_then(|s| serde_json::from_str(&s).ok())
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
             FROM cockpit_events
             WHERE session_id = ?1
               AND json_extract(event_json, '$.ApprovalRequested') IS NOT NULL
               AND json_extract(event_json, '$.ApprovalRequested.approval.nonce') NOT IN (
                   SELECT json_extract(event_json, '$.ApprovalResolved.nonce')
                   FROM cockpit_events
                   WHERE session_id = ?1
                     AND json_extract(event_json, '$.ApprovalResolved') IS NOT NULL
               )",
        ) {
            Ok(s) => s,
            Err(e) => {
                warn!(target: "cockpit.event_store", "prepare unresolved_approval_nonces for {session_id}: {e}");
                return Vec::new();
            }
        };
        let rows = match stmt.query_map(params![session_id], |row| {
            let nonce: String = row.get(0)?;
            Ok(Nonce(nonce))
        }) {
            Ok(r) => r,
            Err(e) => {
                warn!(target: "cockpit.event_store", "query unresolved_approval_nonces for {session_id}: {e}");
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
                "SELECT MAX(seq) FROM cockpit_events
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
                warn!(target: "cockpit.event_store", "has_in_flight_turn prompt query {session_id}: {e}");
                return false;
            }
        };
        let Some(prompt_seq) = prompt_seq else {
            return false;
        };
        let terminator: Option<i64> = match conn
            .query_row(
                "SELECT MIN(seq) FROM cockpit_events
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
                warn!(target: "cockpit.event_store", "has_in_flight_turn terminator query {session_id}: {e}");
                return false;
            }
        };
        terminator.is_none()
    }

    /// Latest `created_at` (ms since epoch) per session for the given
    /// ids, in a single grouped query. Sessions with no events are
    /// absent from the returned map. Backed by the
    /// `(session_id, created_at)` index. Used by the reconciler's
    /// idle-reap pass (#1689) to find cockpit workers that have seen no
    /// activity for longer than `cockpit.auto_stop_idle_secs`, without a
    /// per-session round trip.
    pub fn last_event_at_for_sessions(
        &self,
        session_ids: &[String],
    ) -> std::collections::HashMap<String, i64> {
        let mut out = std::collections::HashMap::new();
        if session_ids.is_empty() {
            return out;
        }
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let placeholders = std::iter::repeat_n("?", session_ids.len())
            .collect::<Vec<_>>()
            .join(",");
        // Exclude non-substantive lifecycle/metadata events so they do not
        // reset the idle clock. AcpSessionAssigned in particular is emitted
        // on every cold-start resume (acp_client.rs), so counting it would
        // make a daemon restart look like fresh activity for every worker
        // and the idle-reap (#1689) would never fire across restarts. Shares
        // NON_SUBSTANTIVE_EVENT_DISCRIMINANTS with the retention prune so the
        // two predicates cannot desync.
        let sql = format!(
            "SELECT session_id, MAX(created_at) FROM cockpit_events
             WHERE session_id IN ({placeholders})
               {clauses}
             GROUP BY session_id",
            clauses = non_substantive_not_like_clauses()
        );
        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(e) => {
                warn!(target: "cockpit.event_store", "last_event_at_for_sessions prepare: {e}");
                return out;
            }
        };
        let rows = stmt.query_map(rusqlite::params_from_iter(session_ids.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        });
        match rows {
            Ok(iter) => {
                for r in iter {
                    match r {
                        Ok((session_id, created_at)) => {
                            out.insert(session_id, created_at);
                        }
                        Err(e) => {
                            warn!(target: "cockpit.event_store", "last_event_at_for_sessions row: {e}");
                        }
                    }
                }
            }
            Err(e) => {
                warn!(target: "cockpit.event_store", "last_event_at_for_sessions query: {e}");
            }
        }
        out
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
        if let Err(e) = conn.execute(
            "INSERT OR IGNORE INTO cockpit_attachments
                (session_id, seq, attachment_id, kind, mime_type, name, data, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                session_id,
                seq as i64,
                blob.id,
                blob.kind.as_str(),
                blob.mime_type,
                blob.name,
                blob.data,
                now_ms,
            ],
        ) {
            warn!(
                target: "cockpit.event_store",
                session = %session_id,
                attachment = %blob.id,
                "insert attachment failed: {e}"
            );
            return false;
        }
        true
    }

    /// Drop all attachment blobs owned by one prompt seq. Used as a
    /// rollback when `UserPromptSent` could not be durably persisted, so
    /// attachment refs and blobs stay in sync.
    pub fn delete_attachments_for_seq(&self, session_id: &str, seq: u64) {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Err(e) = conn.execute(
            "DELETE FROM cockpit_attachments WHERE session_id = ?1 AND seq = ?2",
            params![session_id, seq as i64],
        ) {
            warn!(
                target: "cockpit.event_store",
                session = %session_id,
                seq,
                "rollback attachments failed: {e}"
            );
        }
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
        conn.query_row(
            "SELECT mime_type, data FROM cockpit_attachments
             WHERE session_id = ?1 AND attachment_id = ?2",
            params![session_id, attachment_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()
        .unwrap_or_else(|e| {
            warn!(
                target: "cockpit.event_store",
                session = %session_id,
                attachment = %attachment_id,
                "load attachment failed: {e}"
            );
            None
        })
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
        let json: String = conn
            .query_row(
                "SELECT event_json FROM cockpit_events
                 WHERE session_id = ?1
                   AND event_json LIKE '{\"PromptCapabilities\":%'
                 ORDER BY seq DESC LIMIT 1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten()?;
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
    /// deleted or its substrate is switched away from cockpit, so the
    /// next cockpit_enable starts fresh from seq=1.
    pub fn delete_session(&self, session_id: &str) {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        match conn.execute(
            "DELETE FROM cockpit_events WHERE session_id = ?1",
            params![session_id],
        ) {
            Ok(deleted) => {
                debug!(
                    target: "cockpit.event_store",
                    session = %session_id,
                    deleted,
                    "deleted session events"
                );
            }
            Err(e) => {
                warn!(target: "cockpit.event_store", "delete {session_id}: {e}");
            }
        }
        // Cascade to attachment blobs so a deleted session leaves no
        // orphaned bytes behind.
        if let Err(e) = conn.execute(
            "DELETE FROM cockpit_attachments WHERE session_id = ?1",
            params![session_id],
        ) {
            warn!(target: "cockpit.event_store", "delete attachments {session_id}: {e}");
        }
    }
}

/// A decoded attachment ready to persist: the storage-side counterpart
/// of the wire `PromptAttachmentUpload`. Holds raw bytes (not base64) so
/// the GET endpoint serves them directly. The matching metadata-only
/// `PromptAttachmentRef` is what rides in the event log.
pub struct AttachmentBlob {
    pub id: String,
    pub kind: crate::cockpit::state::PromptAttachmentKind,
    pub mime_type: String,
    pub name: Option<String>,
    pub data: Vec<u8>,
}

/// Cheap discriminant string for `Event` so debug logs don't dump the
/// full payload (assistant chunks can be a few KB each). Unknown
/// variants fall back to "other"; `event_kind` only exists for log
/// breadcrumbs and doesn't need to stay in lockstep with the enum.
fn event_kind(event: &Event) -> &'static str {
    match event {
        Event::PlanUpdated { .. } => "plan_updated",
        Event::TodoListUpdated { .. } => "todo_list_updated",
        Event::ToolCallStarted { .. } => "tool_call_started",
        Event::ToolCallCompleted { .. } => "tool_call_completed",
        Event::ToolCallContent { .. } => "tool_call_content",
        Event::ToolCallUpdated { .. } => "tool_call_updated",
        Event::ApprovalRequested { .. } => "approval_requested",
        Event::ApprovalResolved { .. } => "approval_resolved",
        Event::DiffEmitted { .. } => "diff_emitted",
        Event::ThinkingStarted => "thinking_started",
        Event::ThinkingEnded => "thinking_ended",
        Event::RateLimit { .. } => "rate_limit",
        Event::UsageUpdated { .. } => "usage_updated",
        Event::ModeChanged { .. } => "mode_changed",
        Event::ModesAvailable { .. } => "modes_available",
        Event::CurrentModeChanged { .. } => "current_mode_changed",
        Event::ModeSwitchFailed { .. } => "mode_switch_failed",
        Event::AvailableCommandsUpdated { .. } => "available_commands_updated",
        Event::ConfigOptionsUpdated { .. } => "config_options_updated",
        Event::ConfigOptionSwitchFailed { .. } => "config_option_switch_failed",
        Event::RawAgentUpdate { .. } => "raw_agent_update",
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
        let path = tmp.path().join("cockpit.db");
        let store = EventStore::open(&path, max).unwrap();
        (tmp, store)
    }

    fn img_blob(id: &str) -> AttachmentBlob {
        AttachmentBlob {
            id: id.to_string(),
            kind: crate::cockpit::state::PromptAttachmentKind::Image,
            mime_type: "image/png".into(),
            name: Some("shot.png".into()),
            data: vec![0x89, 0x50, 0x4E, 0x47, 1, 2, 3],
        }
    }

    fn prompt_with_attachment(id: &str) -> Event {
        Event::UserPromptSent {
            text: "look at this".into(),
            attachments: vec![crate::cockpit::state::PromptAttachmentRef {
                id: id.to_string(),
                kind: crate::cockpit::state::PromptAttachmentKind::Image,
                mime_type: "image/png".into(),
                name: Some("shot.png".into()),
                size: 7,
            }],
        }
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
            "INSERT OR IGNORE INTO cockpit_events (session_id, seq, event_json, created_at)
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
                "INSERT INTO cockpit_events (session_id, seq, event_json, created_at)
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
            crate::cockpit::state::PlanStepStatus::Done
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
        // cockpit "Asleep until …" banner whenever the user typed a
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
        let path = tmp.path().join("cockpit.db");
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

    /// `unresolved_approval_nonces` finds `ApprovalRequested` rows whose
    /// nonce never saw a matching `ApprovalResolved`. Used by
    /// `Supervisor::attach` to clear approval cards orphaned by daemon
    /// restart (#1099).
    #[test]
    fn unresolved_approval_nonces_finds_orphaned_requests() {
        use crate::cockpit::approvals::{Approval, ApprovalDecision, Nonce};
        use crate::cockpit::state::ToolCall;

        let (_tmp, store) = open_store(1000);
        let tool_call = ToolCall {
            id: "tc-1".into(),
            name: "Bash".into(),
            kind: "execute".into(),
            args_preview: "ls".into(),
            started_at: Utc::now(),
            parent_tool_call_id: None,
            memory_recall: None,
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
}
