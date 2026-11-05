//! Reading the scoped log back, with cursor progress across unknown payloads.

use std::sync::Arc;

use anyhow::{Context, Result};
use rusqlite::types::Type;
use tracing::{trace, warn};

use super::{EventStore, StoredEvent};
use crate::acp::event_sender::EventMetadata;
use crate::acp::state::Event;
use crate::events::{self, Order, SeqBound};

pub struct ReplayPage {
    pub events: Vec<StoredEvent>,
    pub last_scanned_seq: Option<u64>,
    pub has_more: bool,
    pub highest_seq: u64,
    pub lowest_seq: Option<u64>,
}

impl EventStore {
    pub fn replay_before(&self, session_id: &str, before: u64) -> Result<Vec<StoredEvent>> {
        Ok(self
            .page(session_id, SeqBound::Before(before), Order::Asc, None)?
            .events)
    }

    pub fn replay_from(&self, session_id: &str, since: u64) -> Result<Vec<StoredEvent>> {
        Ok(self.replay_page(session_id, since, None)?.events)
    }

    pub fn replay_page(
        &self,
        session_id: &str,
        since: u64,
        limit: Option<usize>,
    ) -> Result<ReplayPage> {
        let page = self.page(session_id, SeqBound::After(since), Order::Asc, limit)?;
        trace!(target: "acp.event_store", session = %session_id, since, ?limit,
            returned = page.events.len(), has_more = page.has_more, "replayed events");
        Ok(page)
    }

    pub fn replay_page_before(
        &self,
        session_id: &str,
        before: u64,
        limit: Option<usize>,
    ) -> Result<ReplayPage> {
        let mut page = self.page(session_id, SeqBound::Before(before), Order::Desc, limit)?;
        page.events.reverse();
        if page.has_more {
            if let Some(i) = page
                .events
                .iter()
                .position(|stored| is_user_turn_boundary(&stored.event))
                .filter(|&i| i > 0)
            {
                page.events.drain(0..i);
            }
        }
        page.last_scanned_seq = page
            .events
            .first()
            .map(|stored| stored.seq)
            .or(page.last_scanned_seq);
        Ok(page)
    }

    fn page(
        &self,
        session_id: &str,
        bound: SeqBound,
        order: Order,
        limit: Option<usize>,
    ) -> Result<ReplayPage> {
        let conn = self.conn();
        let tx = conn.unchecked_transaction()?;
        let (lowest, highest): (Option<i64>, Option<i64>) = tx.query_row(
            "SELECT MIN(seq),MAX(seq) FROM acp_events WHERE session_id=?1",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let highest_seq = highest
            .map(u64::try_from)
            .transpose()
            .context("negative highest ACP sequence")?
            .unwrap_or(0);
        let lowest_seq = lowest
            .map(u64::try_from)
            .transpose()
            .context("negative lowest ACP sequence")?;
        let mut scanned = 0usize;
        let mut last_scanned_seq = None;
        let mut has_more = false;
        let events = events::scan_projected(
            &tx,
            &self.schema,
            session_id,
            events::ScanWindow {
                bound,
                order,
                limit: limit.map(|n| n.saturating_add(1)),
            },
            "seq,event_json,scope,qualification_json",
            |row| {
                let seq = u64::try_from(row.get::<_, i64>(0)?).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(0, Type::Integer, Box::new(error))
                })?;
                let qualification = row.get_ref(3)?.as_str_or_null().map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(3, Type::Text, Box::new(error))
                })?;
                let metadata =
                    EventMetadata::decode_private(row.get(2)?, qualification).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(3, Type::Text, error.into())
                    })?;
                if limit.is_some_and(|limit| scanned >= limit) {
                    has_more = true;
                    return Ok(None);
                }
                scanned += 1;
                last_scanned_seq = Some(seq);
                let json = row.get_ref(1)?.as_str().map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(1, Type::Text, Box::new(error))
                })?;
                match serde_json::from_str::<Event>(json) {
                    Ok(event) => {
                        metadata.validate_for_event(&event).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(3, Type::Text, error.into())
                        })?;
                        Ok(Some(StoredEvent {
                            seq,
                            event: Arc::new(event),
                            metadata,
                        }))
                    }
                    Err(error) if metadata.availability_denial => Err(
                        rusqlite::Error::FromSqlConversionFailure(1, Type::Text, Box::new(error)),
                    ),
                    Err(error) => {
                        warn!(target: "acp.event_store", session = %session_id, seq, %error,
                            "unknown or malformed historical event");
                        Ok(None)
                    }
                }
            },
        )?;
        tx.commit()?;
        Ok(ReplayPage {
            events,
            last_scanned_seq,
            has_more,
            highest_seq,
            lowest_seq,
        })
    }
}

fn is_user_turn_boundary(event: &Event) -> bool {
    matches!(
        event,
        Event::UserPromptSent { .. } | Event::UserDiffCommentsPrompt { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;

    #[test]
    fn legacy_prompt_errors_replay_without_rewriting_or_inventing_evidence() {
        use crate::acp::state::test_support::{chunk, prompt, stopped};
        use crate::acp::transcript::{TranscriptModel, TranscriptRowKind};
        let (_tmp, store) = open_store(1000);
        for (id, json) in [
            (
                "missing",
                r#"{"PromptRuntimeError":{"message":"request failed"}}"#,
            ),
            (
                "null",
                r#"{"PromptRuntimeError":{"message":"request failed","evidence":null}}"#,
            ),
        ] {
            store
                .record(
                    id,
                    1,
                    &Event::AcpSessionAssigned {
                        acp_session_id: "native-history".into(),
                        connection_ready: false,
                        native_subject: None,
                    },
                    &crate::acp::event_sender::EventMetadata::history(),
                )
                .unwrap();
            store
                .record(
                    id,
                    2,
                    &prompt("explicit request"),
                    &crate::acp::event_sender::EventMetadata::history(),
                )
                .unwrap();
            store
                .record(
                    id,
                    3,
                    &chunk("partial output survives"),
                    &crate::acp::event_sender::EventMetadata::history(),
                )
                .unwrap();
            store.conn().execute(
                "INSERT INTO acp_events (session_id, seq, event_json, created_at) VALUES (?1, 4, ?2, 0)",
                rusqlite::params![id, json],
            ).unwrap();
            store
                .record(
                    id,
                    5,
                    &stopped("prompt_failed"),
                    &crate::acp::event_sender::EventMetadata::history(),
                )
                .unwrap();
            let replay = store.replay_page(id, 0, Some(10)).unwrap();
            assert_eq!(replay.events.len(), 5);
            assert!(matches!(
                replay.events[3].event.as_ref(),
                Event::PromptRuntimeError { evidence: None, .. }
            ));
            let mut transcript = TranscriptModel::new();
            for stored in replay.events {
                transcript.apply_scoped_event(stored.seq, stored.event.as_ref(), &stored.metadata);
            }
            assert!(transcript
                .rows()
                .iter()
                .any(|row| row.text == "partial output survives"));
            assert!(transcript
                .rows()
                .iter()
                .any(|row| row.kind == TranscriptRowKind::Notice
                    && row.text.contains("request failed")));
            assert!(!transcript
                .rows()
                .iter()
                .any(|row| row.kind == TranscriptRowKind::EmptyOutput));
            let retained: String = store
                .conn()
                .query_row(
                    "SELECT event_json FROM acp_events WHERE session_id = ?1 AND seq = 4",
                    [id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(retained, json);
        }
    }

    #[test]
    fn forward_pages_reassemble_the_full_replay() {
        let (_tmp, store) = open_store(1000);
        record_from(&store, "s-1", 1, (1..=10).map(|_| Event::ThinkingStarted));
        assert_eq!(seqs(&store.replay_from("s-1", 7).unwrap()), [8, 9, 10]);
        assert_eq!(seqs(&store.replay_before("s-1", 3).unwrap()), [1, 2]);

        let mut paged = Vec::new();
        let mut cursor = 0u64;
        loop {
            let page = store.replay_page("s-1", cursor, Some(3)).unwrap();
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
            let page = store.replay_page("s-1", 0, Some(limit)).unwrap();
            assert_eq!(
                (page.events.len(), page.has_more, page.last_scanned_seq),
                (len, has_more, Some(cursor)),
                "limit {limit}"
            );
        }

        let past_end = store.replay_page("s-1", u64::MAX, Some(1000)).unwrap();
        assert!(past_end.events.is_empty() && !past_end.has_more);
        assert_eq!((past_end.highest_seq, past_end.lowest_seq), (10, Some(1)));

        // The cursor advances past a corrupt row.
        store
            .record(
                "bad",
                1,
                &Event::ThinkingStarted,
                &crate::acp::event_sender::EventMetadata::history(),
            )
            .unwrap();
        store
            .conn()
            .execute(
                "INSERT INTO acp_events (session_id, seq, event_json, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params!["bad", 2_i64, "{not valid event json", 0_i64],
            )
            .unwrap();
        store
            .record(
                "bad",
                3,
                &Event::ThinkingEnded,
                &crate::acp::event_sender::EventMetadata::history(),
            )
            .unwrap();

        let page1 = store.replay_page("bad", 0, Some(2)).unwrap();
        assert_eq!(seqs(&page1.events), [1]);
        assert_eq!(page1.last_scanned_seq, Some(2));
        assert!(page1.has_more);
        let page2 = store.replay_page("bad", 2, Some(2)).unwrap();
        assert_eq!(seqs(&page2.events), [3]);
        assert!(!page2.has_more);
    }

    #[test]
    fn backward_pages_return_the_closest_rows_and_trim_to_a_turn_boundary() {
        let (_tmp, store) = open_store(1000);
        record_from(&store, "s-1", 1, (1..=10).map(|_| Event::ThinkingStarted));
        let tail = store.replay_page_before("s-1", u64::MAX, Some(3)).unwrap();
        assert_eq!(seqs(&tail.events), [8, 9, 10]);
        assert!(tail.has_more);
        assert_eq!((tail.last_scanned_seq, tail.highest_seq), (Some(8), 10));
        let older = store.replay_page_before("s-1", 8, Some(3)).unwrap();
        assert_eq!(seqs(&older.events), [5, 6, 7]);
        assert_eq!(older.last_scanned_seq, Some(5));
        let start = store.replay_page_before("s-1", 3, Some(10)).unwrap();
        assert_eq!(seqs(&start.events), [1, 2]);
        assert!(!start.has_more, "reached session start");
        assert!(store
            .replay_page_before("s-1", 1, Some(10))
            .unwrap()
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
        let tail = store.replay_page_before("s-2", u64::MAX, Some(4)).unwrap();
        assert_eq!(
            seqs(&tail.events),
            [5, 6, 7],
            "leading partial turn trimmed"
        );
        assert!(tail.has_more);
        assert_eq!(tail.last_scanned_seq, Some(5), "cursor is boundary B");
        let older = store.replay_page_before("s-2", 5, Some(10)).unwrap();
        assert_eq!(seqs(&older.events), [1, 2, 3, 4]);
        assert!(!older.has_more);
    }
}
