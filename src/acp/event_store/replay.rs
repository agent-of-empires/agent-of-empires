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

impl EventStore {
    /// All events with `seq < before`, oldest first.
    pub fn replay_before(&self, session_id: &str, before: u64) -> Vec<(u64, Event)> {
        let rows = events::scan(
            &self.conn(),
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

    fn page(
        &self,
        session_id: &str,
        bound: SeqBound,
        order: Order,
        limit: Option<usize>,
    ) -> ReplayPage {
        let conn = self.conn();
        let highest_seq = events::highest_seq(&conn, &self.schema, session_id);
        let lowest_seq = events::lowest_seq(&conn, &self.schema, session_id);
        // One probe row past `limit` detects `has_more` without a second query.
        let probe = limit.map(|n| n.saturating_add(1));
        let rows = events::scan(&conn, &self.schema, session_id, bound, order, probe);
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
