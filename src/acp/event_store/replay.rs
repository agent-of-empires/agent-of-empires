//! Reading the log back: full replays and cursor pages in either direction.

use tracing::{trace, warn};

use super::EventStore;
use crate::acp::state::Event;
use crate::events::{self, Order, SeqBound};

/// One page of replayed events plus the cursor metadata for the next page.
pub struct ReplayPage {
    /// Deserialised events for this page, oldest first.
    pub events: Vec<(u64, Event)>,
    /// Cursor for the next page: the last seq this page consumed, decodable or not.
    pub last_scanned_seq: Option<u64>,
    /// True when at least one row exists beyond this page's window.
    pub has_more: bool,
    /// Highest seq stored for the session, or 0 if none.
    pub highest_seq: u64,
    /// Lowest seq still stored, or `None` when empty.
    pub lowest_seq: Option<u64>,
}

/// Rows per page of a paged replay: the connection is held for one page at a time.
const REPLAY_PAGE_ROWS: usize = 2000;

impl EventStore {
    /// All events with `seq < before`, oldest first.
    pub fn replay_before(&self, session_id: &str, before: u64) -> Vec<(u64, Event)> {
        let rows = events::scan(
            &self.reader(),
            &self.schema,
            session_id,
            SeqBound::Before(before),
            Order::Asc,
            None,
        );
        decode_rows(session_id, rows, None).0
    }

    /// All events with `seq > since`, oldest first.
    pub fn replay_from(&self, session_id: &str, since: u64) -> Vec<(u64, Event)> {
        self.replay_page(session_id, since, None).events
    }

    /// Hand every event with `seq > since` to `visit`, oldest first, one page at a time.
    ///
    /// The connection mutex is released between pages, so a long history never blocks other
    /// sessions' reads and writes for the whole scan the way `replay_from` does. Returns the
    /// last seq visited, or `since` when there was nothing to visit.
    pub fn replay_each_paged(
        &self,
        session_id: &str,
        since: u64,
        mut visit: impl FnMut(u64, Event),
    ) -> u64 {
        let mut cursor = since;
        let mut last = since;
        loop {
            let page = self.replay_page(session_id, cursor, Some(REPLAY_PAGE_ROWS));
            for (seq, event) in page.events {
                last = seq;
                visit(seq, event);
            }
            match page.last_scanned_seq {
                Some(next) if page.has_more => cursor = next,
                _ => return last,
            }
            // Let a waiting writer take the connection before the next page.
            std::thread::yield_now();
        }
    }

    /// Up to `limit` events with `seq > since`, oldest first.
    pub fn replay_page(&self, session_id: &str, since: u64, limit: Option<usize>) -> ReplayPage {
        let page = self.page(session_id, SeqBound::After(since), Order::Asc, limit);
        trace!(
            target: "acp.event_store",
            session = %session_id,
            since,
            limit = ?limit,
            returned = page.events.len(),
            has_more = page.has_more,
            "replayed events"
        );
        page
    }

    /// Up to `limit` events closest below `before`, oldest first. When older
    /// events remain, a leading partial turn is trimmed so pages start on a
    /// user turn boundary.
    pub fn replay_page_before(
        &self,
        session_id: &str,
        before: u64,
        limit: Option<usize>,
    ) -> ReplayPage {
        let mut page = self.page(session_id, SeqBound::Before(before), Order::Desc, limit);
        page.events.reverse();
        if page.has_more {
            if let Some(i) = page
                .events
                .iter()
                .position(|(_, ev)| is_user_turn_boundary(ev))
                .filter(|&i| i > 0)
            {
                page.events.drain(0..i);
            }
        }
        // The next-older page starts below the lowest seq kept, so a trimmed turn reloads whole.
        page.last_scanned_seq = page
            .events
            .first()
            .map(|(seq, _)| *seq)
            .or(page.last_scanned_seq);
        trace!(
            target: "acp.event_store",
            session = %session_id,
            before,
            limit = ?limit,
            returned = page.events.len(),
            has_more = page.has_more,
            "replayed events before cursor"
        );
        page
    }

    /// Read one coherent retained page; `None` means the expected identity changed.
    pub(crate) fn retained_page(
        &self,
        session_id: &str,
        since: u64,
        before: Option<u64>,
        limit: usize,
        expected_identity: Option<&str>,
    ) -> anyhow::Result<Option<(ReplayPage, String)>> {
        let (rows, highest_seq, lowest_seq, identity) = {
            let mut conn = self.read_conn();
            let tx = conn.transaction()?;
            let identity = super::identity::read(&tx, session_id)?;
            if expected_identity.is_some_and(|expected| expected != identity) {
                return Ok(None);
            }
            #[cfg(test)]
            self.pause_retained_read(session_id, super::test_support::RetainedReadStage::Snapshot);
            let table = self.schema.events_table();
            let (highest, lowest): (Option<i64>, Option<i64>) = tx.query_row(
                &format!(
                    "SELECT
                   (SELECT seq FROM {table} WHERE session_id = ?1 ORDER BY seq DESC LIMIT 1),
                   (SELECT seq FROM {table} WHERE session_id = ?1 ORDER BY seq ASC LIMIT 1)"
                ),
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let highest_seq = highest.map(u64::try_from).transpose()?.unwrap_or(0);
            let lowest_seq = lowest.map(u64::try_from).transpose()?;
            let (comparison, order, cursor) = match before {
                Some(before) if before > i64::MAX as u64 => ("<=", "DESC", before),
                Some(before) => ("<", "DESC", before),
                None => (">", "ASC", since),
            };
            let rows = {
                let mut statement = tx.prepare(&format!(
                    "SELECT seq, event_json FROM {table}
                 WHERE session_id = ?1 AND seq {comparison} ?2
                 ORDER BY seq {order} LIMIT ?3"
                ))?;
                let found = statement.query_map(
                    rusqlite::params![
                        session_id,
                        i64::try_from(cursor).unwrap_or(i64::MAX),
                        i64::try_from(limit.saturating_add(1))?
                    ],
                    |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, String>(1)?)),
                )?;
                found.collect::<rusqlite::Result<Vec<_>>>()?
            };
            tx.commit()?;
            (rows, highest_seq, lowest_seq, identity)
        };
        #[cfg(test)]
        self.pause_retained_read(
            session_id,
            super::test_support::RetainedReadStage::BeforeDecode,
        );
        let (mut events, last_scanned_seq, has_more) = decode_rows(session_id, rows, Some(limit));
        if before.is_some() {
            events.reverse();
        }
        Ok(Some((
            ReplayPage {
                events,
                last_scanned_seq,
                has_more,
                highest_seq,
                lowest_seq,
            },
            identity,
        )))
    }

    fn page(
        &self,
        session_id: &str,
        bound: SeqBound,
        order: Order,
        limit: Option<usize>,
    ) -> ReplayPage {
        let conn = self.reader();
        // One snapshot, so the bounds agree with the rows while writes land.
        let snapshot = conn
            .unchecked_transaction()
            .map_err(|e| warn!(target: "acp.event_store", "replay snapshot for {session_id}: {e}"))
            .ok();
        let highest_seq = events::highest_seq(&conn, &self.schema, session_id);
        let lowest_seq = events::lowest_seq(&conn, &self.schema, session_id);
        #[cfg(test)]
        tests::AFTER_BOUNDS.with(|hook| hook.take().map(|run| run()));
        // One probe row past `limit` detects `has_more` without a second query.
        let probe = limit.map(|n| n.saturating_add(1));
        let rows = events::scan(&conn, &self.schema, session_id, bound, order, probe);
        drop(snapshot);
        let (events, last_scanned_seq, has_more) = decode_rows(session_id, rows, limit);
        ReplayPage {
            events,
            last_scanned_seq,
            has_more,
            highest_seq,
            lowest_seq,
        }
    }
}

/// Decode up to `limit` rows, logging corrupt ones; returns the events, the
/// last seq consumed, and whether a probe row beyond `limit` was present.
fn decode_rows(
    session_id: &str,
    rows: Vec<(u64, String)>,
    limit: Option<usize>,
) -> (Vec<(u64, Event)>, Option<u64>, bool) {
    let has_more = limit.is_some_and(|n| rows.len() > n);
    let mut out = Vec::new();
    let mut last_scanned_seq = None;
    for (seq, json) in rows.into_iter().take(limit.unwrap_or(usize::MAX)) {
        last_scanned_seq = Some(seq);
        match serde_json::from_str::<Event>(&json) {
            Ok(event) => out.push((seq, event)),
            Err(e) => warn!(
                target: "acp.event_store",
                "deserialise event {session_id}@{seq}: {e}"
            ),
        }
    }
    (out, last_scanned_seq, has_more)
}

fn is_user_turn_boundary(ev: &Event) -> bool {
    matches!(
        ev,
        Event::UserPromptSent { .. } | Event::UserDiffCommentsPrompt { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;

    #[tokio::test]
    async fn fixture_event_store_survives_app_state_drop() {
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        let store = std::sync::Arc::clone(&state.acp_event_store);
        store
            .record("retained", 1, &agent_chunk("retained needle"))
            .unwrap();
        drop(state);
        store.record("retained", 2, &Event::ThinkingEnded).unwrap();
        let (page, _) = store
            .retained_page("retained", 0, None, 8, None)
            .unwrap()
            .unwrap();
        assert_eq!(seqs(&page.events), [1, 2]);
        assert_eq!((page.highest_seq, page.lowest_seq), (2, Some(1)));
        let hits = store.search_content("needle", 1);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "retained");
    }

    #[tokio::test]
    async fn retained_snapshot_reads_do_not_block_publication_or_other_readers() {
        use crate::acp::supervisor::{BroadcastSink, ChannelSink};
        use axum::{body::Body, http::Request, routing::get, Router};
        use std::sync::{mpsc, Arc};
        use std::time::Duration;
        use tower::ServiceExt;

        for stage in [RetainedReadStage::Snapshot, RetainedReadStage::BeforeDecode] {
            let instance = crate::session::Instance::new("concurrent retained snapshot", "/tmp");
            let id = instance.id.clone();
            let state = crate::server::test_support::build_test_app_state(vec![instance]);
            let store = Arc::clone(&state.acp_event_store);
            store
                .record(&id, 1, &user_prompt("original snapshot"))
                .unwrap();
            let (_, initial_identity) =
                store.retained_page(&id, 0, None, 8, None).unwrap().unwrap();
            let (entered, release) = gate_retained_read(&store, &id, stage);
            let reader_store = Arc::clone(&store);
            let reader_id = id.clone();
            let reader = std::thread::spawn(move || {
                reader_store.retained_page(&reader_id, 0, None, 8, Some(&initial_identity))
            });
            let deadline = Duration::from_secs(5);
            entered
                .recv_timeout(deadline)
                .expect("retained reader must reach its gate");

            let (tx, mut broadcasts) = tokio::sync::broadcast::channel(4);
            let sink = ChannelSink::new(
                tx,
                Arc::clone(&store),
                Arc::new(crate::acp::control_cache::ControlStateCache::new()),
            );
            let (done, completed) = mpsc::channel();
            let writer_id = id.clone();
            let writer = std::thread::spawn(move || {
                let persisted = sink.publish_persisted("unrelated", 1, &Event::ThinkingStarted)
                    && sink.publish_persisted(&writer_id, 2, &Event::ThinkingEnded);
                done.send(persisted).unwrap();
            });
            let publication = completed.recv_timeout(deadline);
            let second = (stage == RetainedReadStage::BeforeDecode).then(|| {
                let store = Arc::clone(&store);
                let id = id.clone();
                let (done, completed) = mpsc::channel();
                let reader = std::thread::spawn(move || {
                    let page = store.retained_page(&id, 0, None, 8, None);
                    done.send(()).unwrap();
                    page
                });
                let completion = completed.recv_timeout(deadline);
                (reader, completion)
            });
            release.send(()).unwrap();
            let (page, identity) = reader.join().unwrap().unwrap().unwrap();
            writer.join().unwrap();
            assert!(publication.expect("publication must finish before the reader is released"));
            assert_eq!(broadcasts.try_recv().unwrap().session_id, "unrelated");
            let published = broadcasts.try_recv().unwrap();
            assert_eq!(
                (published.session_id.as_str(), published.seq),
                (id.as_str(), 2)
            );
            assert!(matches!(published.event.as_ref(), Event::ThinkingEnded));
            assert_eq!(seqs(&store.replay_from("unrelated", 0)), [1]);
            assert_eq!(seqs(&page.events), [1]);
            assert!(matches!(&page.events[0].1, Event::UserPromptSent { .. }));
            assert_eq!(
                (page.highest_seq, page.lowest_seq, page.last_scanned_seq),
                (1, Some(1), Some(1))
            );
            assert!(!page.has_more);
            if let Some((reader, completion)) = second {
                let (latest, latest_identity) = reader.join().unwrap().unwrap().unwrap();
                completion.expect("another retained read must finish before decoding resumes");
                assert_eq!(seqs(&latest.events), [1, 2]);
                assert_ne!(latest_identity, identity);
            }
            let app = Router::new()
                .route(
                    "/api/sessions/{id}/acp/replay",
                    get(crate::server::api::acp_replay),
                )
                .with_state(state);
            let response = app.oneshot(Request::builder()
                .uri(format!("/api/sessions/{id}/acp/replay?view=retained&history_identity={identity}"))
                .body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
        }
    }

    #[test]
    fn retained_and_search_reads_share_private_memory_and_temporary_databases() {
        for name in [":memory:", ""] {
            let store = EventStore::open(std::path::Path::new(name), 0).unwrap();
            store
                .record("private", 1, &agent_chunk("private needle"))
                .unwrap();
            let (page, identity) = store
                .retained_page("private", 0, None, 8, None)
                .unwrap()
                .unwrap();
            assert_eq!(seqs(&page.events), [1]);
            assert_eq!((page.highest_seq, page.lowest_seq), (1, Some(1)));
            let hits = store.search_content("needle", 1);
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].session_id, "private");
            store.record("private", 2, &Event::ThinkingEnded).unwrap();
            assert!(store
                .retained_page("private", 0, None, 8, Some(&identity))
                .unwrap()
                .is_none());
            let (latest, _) = store
                .retained_page("private", 0, None, 8, None)
                .unwrap()
                .unwrap();
            assert_eq!(seqs(&latest.events), [1, 2]);
        }
    }

    #[test]
    fn backward_cursors_above_sqlite_max_include_maximum_sequence() {
        let (_tmp, store) = open_store(0);
        let max = i64::MAX as u64;
        store
            .record("boundary", max - 1, &Event::ThinkingStarted)
            .unwrap();
        store
            .record("boundary", max, &Event::ThinkingEnded)
            .unwrap();
        for (cursor, expected) in [
            (max, vec![max - 1]),
            (max + 1, vec![max - 1, max]),
            (u64::MAX, vec![max - 1, max]),
        ] {
            assert_eq!(seqs(&store.replay_before("boundary", cursor)), expected);
            let (page, _) = store
                .retained_page("boundary", 0, Some(cursor), 10, None)
                .unwrap()
                .unwrap();
            assert_eq!(seqs(&page.events), expected);
            assert!(!page.has_more);
        }
    }
    thread_local! {
        /// Runs once between a page's bounds queries and its row scan.
        pub(super) static AFTER_BOUNDS: std::cell::Cell<Option<Box<dyn FnOnce()>>> =
            const { std::cell::Cell::new(None) };
    }

    #[test]
    fn forward_pages_reassemble_the_full_replay() {
        let (_tmp, store) = open_store(1000);
        record_from(&store, "s-1", 1, (1..=10).map(|_| Event::ThinkingStarted));
        assert_eq!(seqs(&store.replay_from("s-1", 7)), [8, 9, 10]);
        assert_eq!(seqs(&store.replay_before("s-1", 3)), [1, 2]);

        let mut paged = Vec::new();
        let mut cursor = 0u64;
        loop {
            let page = store.replay_page("s-1", cursor, Some(3));
            assert!(page.events.len() <= 3, "page exceeded limit");
            paged.extend(seqs(&page.events));
            match page.last_scanned_seq {
                Some(next) if page.has_more => cursor = next,
                _ => break,
            }
        }
        assert_eq!(paged, (1..=10).collect::<Vec<_>>());

        // (limit, events, has_more, cursor) at and around the exact boundary.
        for (limit, len, has_more, cursor) in [(10, 10, false, 10), (9, 9, true, 9)] {
            let page = store.replay_page("s-1", 0, Some(limit));
            assert_eq!(
                (page.events.len(), page.has_more, page.last_scanned_seq),
                (len, has_more, Some(cursor)),
                "limit {limit}"
            );
        }

        let past_end = store.replay_page("s-1", u64::MAX, Some(1000));
        assert!(past_end.events.is_empty() && !past_end.has_more);
        assert_eq!((past_end.highest_seq, past_end.lowest_seq), (10, Some(1)));

        // The cursor advances past a corrupt row.
        store.record("bad", 1, &Event::ThinkingStarted).unwrap();
        store
            .conn()
            .execute(
                "INSERT INTO acp_events (session_id, seq, event_json, created_at)
             VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params!["bad", 2_i64, "{not valid event json", 0_i64],
            )
            .unwrap();
        store.record("bad", 3, &Event::ThinkingEnded).unwrap();

        let page1 = store.replay_page("bad", 0, Some(2));
        assert_eq!(seqs(&page1.events), [1]);
        assert_eq!(page1.last_scanned_seq, Some(2));
        assert!(page1.has_more);
        let page2 = store.replay_page("bad", 2, Some(2));
        assert_eq!(seqs(&page2.events), [3]);
        assert!(!page2.has_more);
    }

    /// The scan must take the connection one page at a time: a row recorded while the first
    /// page is visited is picked up by a later page, which one long scan could not see.
    #[test]
    fn paged_replay_reads_each_page_under_its_own_connection() {
        let (_tmp, store) = open_store(100_000);
        let total = (REPLAY_PAGE_ROWS * 3 + 5) as u64;
        record_from(
            &store,
            "cold",
            1,
            (0..total).map(|_| Event::ThinkingStarted),
        );
        let mut seen = Vec::new();
        let last = store.replay_each_paged("cold", 0, |seq, _| {
            seen.push(seq);
            if seq == 1 {
                store
                    .record("cold", total + 1, &Event::ThinkingStarted)
                    .unwrap();
            }
        });
        assert_eq!(seen, (1..=total + 1).collect::<Vec<_>>());
        assert_eq!(last, total + 1);
        assert_eq!(store.replay_each_paged("empty", 7, |_, _| {}), 7);
    }

    #[test]
    fn a_page_reads_its_bounds_and_rows_from_one_snapshot() {
        let (tmp, store) = open_store(1000);
        record_from(&store, "s-1", 1, (1..=3).map(|_| Event::ThinkingStarted));
        let db = tmp.path().join("acp.db");
        AFTER_BOUNDS.with(|hook| {
            hook.set(Some(Box::new(move || {
                // A separate connection, as the event writer's would be.
                let other = EventStore::open(&db, 1000).unwrap();
                other.record("s-1", 4, &Event::ThinkingEnded).unwrap();
            })))
        });
        let page = store.replay_page("s-1", 0, None);
        assert_eq!(
            (seqs(&page.events), page.highest_seq),
            (vec![1, 2, 3], 3),
            "a row committed after the bounds must stay out of this page"
        );
        assert_eq!(store.highest_seq("s-1"), 4, "the concurrent commit landed");
    }

    #[test]
    fn replays_do_not_wait_for_the_writer_connection() {
        let (_tmp, store) = open_store(1000);
        record_from(&store, "s-1", 1, (1..=3).map(|_| Event::ThinkingStarted));
        let writer = store.conn();
        let (done, replayed) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let page = store.replay_page("s-1", 0, Some(10));
                let _ = done.send((
                    seqs(&page.events),
                    page.highest_seq,
                    store.lowest_seq("s-1"),
                ));
            });
            let result = replayed.recv_timeout(std::time::Duration::from_secs(5));
            drop(writer);
            assert_eq!(
                result.expect("a replay must not wait while the writer is held"),
                (vec![1, 2, 3], 3, Some(1))
            );
        });
    }

    #[test]
    fn backward_pages_return_the_closest_rows_and_trim_to_a_turn_boundary() {
        let (_tmp, store) = open_store(1000);
        record_from(&store, "s-1", 1, (1..=10).map(|_| Event::ThinkingStarted));
        let tail = store.replay_page_before("s-1", u64::MAX, Some(3));
        assert_eq!(seqs(&tail.events), [8, 9, 10]);
        assert!(tail.has_more);
        assert_eq!((tail.last_scanned_seq, tail.highest_seq), (Some(8), 10));
        let older = store.replay_page_before("s-1", 8, Some(3));
        assert_eq!(seqs(&older.events), [5, 6, 7]);
        assert_eq!(older.last_scanned_seq, Some(5));
        let start = store.replay_page_before("s-1", 3, Some(10));
        assert_eq!(seqs(&start.events), [1, 2]);
        assert!(!start.has_more, "reached session start");
        assert!(store
            .replay_page_before("s-1", 1, Some(10))
            .events
            .is_empty());

        let capabilities = Event::PromptCapabilities {
            image: true,
            audio: false,
            embedded_context: true,
            load_session: None,
            steering: false,
        };
        record_from(
            &store,
            "s-2",
            1,
            [
                capabilities,
                user_prompt("A"),
                Event::ThinkingStarted,
                Event::ThinkingStarted,
                user_prompt("B"),
                Event::ThinkingStarted,
                Event::ThinkingStarted,
            ],
        );
        // A tail of 4 would start at seq 4, mid-turn A.
        let tail = store.replay_page_before("s-2", u64::MAX, Some(4));
        assert_eq!(
            seqs(&tail.events),
            [5, 6, 7],
            "leading partial turn trimmed"
        );
        assert!(tail.has_more);
        assert_eq!(tail.last_scanned_seq, Some(5), "cursor is boundary B");
        let older = store.replay_page_before("s-2", 5, Some(10));
        assert_eq!(seqs(&older.events), [1, 2, 3, 4]);
        assert!(!older.has_more);
    }
}
