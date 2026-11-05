//! Qualified rate-limit parks and the durable redelivery budget.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use super::{ControlKnowledge, EventStore};
use crate::acp::state::{Event, RateLimitInfo};
use crate::events;

#[derive(Debug, Clone)]
pub struct RateLimitPark {
    pub seq: u64,
    pub info: Option<RateLimitInfo>,
    pub recorded_at_ms: i64,
    pub cap_reached: bool,
    pub last_resume_attempt_ms: Option<i64>,
}

fn control_event(conn: &Connection, session_id: &str, seq: u64) -> Result<Option<Event>> {
    let json: Option<String> = conn
        .query_row(
            "SELECT event_json FROM acp_events WHERE session_id=?1 AND seq=?2",
            params![session_id, i64::try_from(seq)?],
            |row| row.get(0),
        )
        .optional()?;
    json.map(|json| {
        serde_json::from_str(&json)
            .with_context(|| format!("invalid ACP control event at {session_id}@{seq}"))
    })
    .transpose()
}

fn quota_at(conn: &Connection, session_id: &str, seq: u64) -> Result<Option<RateLimitInfo>> {
    match control_event(conn, session_id, seq)? {
        Some(Event::RateLimit { info }) => Ok(Some(info)),
        None => Ok(None),
        _ => anyhow::bail!("quota anchor does not identify a RateLimit event"),
    }
}

impl EventStore {
    pub fn latest_rate_limit_event(
        &self,
        session_id: &str,
    ) -> Result<Option<(RateLimitInfo, i64)>> {
        self.with_control_snapshot(session_id, |conn, cursor| {
            let Some(quota) = cursor.projection.last_quota else {
                return Ok(None);
            };
            Ok(quota_at(conn, session_id, quota.seq)?.map(|info| (info, quota.created_at_ms)))
        })
    }

    /// Read quota park knowledge and its exact projection frontier without loading payloads.
    pub fn rate_limit_park_snapshot(
        &self,
        session_id: &str,
    ) -> Result<(u64, ControlKnowledge<Option<bool>>)> {
        self.with_control_snapshot(session_id, |_, cursor| {
            let park = match cursor.projection.park {
                ControlKnowledge::Known(park) => {
                    ControlKnowledge::Known(park.map(|park| park.cap_reached))
                }
                ControlKnowledge::Indeterminate => ControlKnowledge::Indeterminate,
            };
            Ok((cursor.seq, park))
        })
    }

    pub fn rate_limit_park(
        &self,
        session_id: &str,
    ) -> Result<ControlKnowledge<Option<RateLimitPark>>> {
        self.with_control_snapshot(session_id, |conn, cursor| {
            Ok(match cursor.projection.park {
                ControlKnowledge::Known(Some(park)) => {
                    ControlKnowledge::Known(Some(RateLimitPark {
                        seq: park.seq,
                        info: match park.quota {
                            Some(quota) => quota_at(conn, session_id, quota.seq)?,
                            None => None,
                        },
                        recorded_at_ms: park.recorded_at_ms,
                        cap_reached: park.cap_reached,
                        last_resume_attempt_ms: park.last_resume_attempt_ms,
                    }))
                }
                ControlKnowledge::Known(None) => ControlKnowledge::Known(None),
                ControlKnowledge::Indeterminate => ControlKnowledge::Indeterminate,
            })
        })
    }

    pub fn rate_limited_turn_prompt(
        &self,
        session_id: &str,
    ) -> Result<ControlKnowledge<Option<(String, Vec<crate::daemon::PromptAttachmentRef>)>>> {
        self.with_control_snapshot(session_id, |conn, cursor| {
            Ok(match cursor.projection.interrupted_prompt {
                ControlKnowledge::Known(Some(seq)) => match control_event(conn, session_id, seq)? {
                    Some(Event::UserPromptSent {
                        text, attachments, ..
                    }) => ControlKnowledge::Known(Some((text, attachments))),
                    Some(Event::UserDiffCommentsPrompt {
                        assembled_markdown, ..
                    }) => ControlKnowledge::Known(Some((assembled_markdown, Vec::new()))),
                    None => ControlKnowledge::Indeterminate,
                    _ => anyhow::bail!("interrupted prompt anchor does not identify an input"),
                },
                ControlKnowledge::Known(None) => ControlKnowledge::Known(None),
                ControlKnowledge::Indeterminate => ControlKnowledge::Indeterminate,
            })
        })
    }

    pub fn rate_limit_interrupted_agent_turn(
        &self,
        session_id: &str,
    ) -> Result<ControlKnowledge<bool>> {
        self.with_control_snapshot(session_id, |_, cursor| {
            Ok(cursor.projection.interrupted_agent)
        })
    }

    pub fn rate_limit_redelivery_streak(&self, session_id: &str) -> Result<ControlKnowledge<i64>> {
        self.with_control_snapshot(session_id, |_, cursor| {
            Ok(match cursor.projection.budget {
                ControlKnowledge::Known(budget) => ControlKnowledge::Known(budget.spent),
                ControlKnowledge::Indeterminate => ControlKnowledge::Indeterminate,
            })
        })
    }
}

pub(super) fn preserve_legacy_budgets(
    conn: &rusqlite::Transaction<'_>,
    schema: &events::Schema,
) -> Result<()> {
    let table = schema.rate_limit_budgets_table();
    let mut existing = conn.prepare(&format!("SELECT session_id, spent, armed FROM {table}"))?;
    let mut rows = existing.query([])?;
    while let Some(row) = rows.next()? {
        let _: String = row.get(0)?;
        let spent: i64 = row.get(1)?;
        let armed: i64 = row.get(2)?;
        anyhow::ensure!(
            spent >= 0 && matches!(armed, 0 | 1),
            "invalid durable rate-limit budget"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::acp::state::RATE_LIMIT_EXHAUSTED_RETRIES_REASON;
    use chrono::Utc;
    use ControlKnowledge::{Indeterminate, Known};

    fn rate_limit_event(secs_until_reset: i64) -> Event {
        Event::RateLimit {
            info: RateLimitInfo {
                status: "usage limit reached".into(),
                resets_at: Some(Utc::now() + chrono::Duration::seconds(secs_until_reset)),
                kind: "rate_limit".into(),
            },
        }
    }

    fn resume(manual: bool) -> Event {
        Event::RateLimitAutoResumed {
            resets_at: Utc::now(),
            manual,
        }
    }

    #[test]
    fn quota_snapshot_tracks_canonical_park_frontier_across_history_and_reopen() {
        let (tmp, store) = open_store(0);
        assert_eq!(
            store.rate_limit_park_snapshot("s").unwrap(),
            (0, Known(None))
        );
        record_projection(
            &store,
            "s",
            1,
            [Event::ThinkingStarted, rate_limit_event(60)],
        );
        assert_eq!(
            store.rate_limit_park_snapshot("s").unwrap(),
            (2, Known(Some(false)))
        );
        record_from(
            &store,
            "s",
            3,
            [
                stopped(RATE_LIMIT_EXHAUSTED_RETRIES_REASON),
                user_prompt("historical input"),
                resume(false),
            ],
        );
        assert_eq!(
            store.rate_limit_park_snapshot("s").unwrap(),
            (5, Known(Some(false)))
        );
        record_projection(
            &store,
            "s",
            6,
            [stopped(RATE_LIMIT_EXHAUSTED_RETRIES_REASON)],
        );
        assert_eq!(
            store.rate_limit_park_snapshot("s").unwrap(),
            (6, Known(Some(true)))
        );
        drop(store);
        let cold = EventStore::open(&tmp.path().join("acp.db"), 0).unwrap();
        assert_eq!(
            cold.rate_limit_park_snapshot("s").unwrap(),
            (6, Known(Some(true)))
        );
        record_projection(&cold, "s", 7, [user_prompt("new explicit input")]);
        assert_eq!(
            cold.rate_limit_park_snapshot("s").unwrap(),
            (7, Known(None))
        );

        record_from(&cold, "legacy", 1, [stopped("rate_limited")]);
        cold.control_cache.forget("legacy");
        assert_eq!(
            cold.rate_limit_park_snapshot("legacy").unwrap(),
            (1, Indeterminate)
        );
    }

    #[test]
    fn qualified_parks_preserve_limit_and_resume_attempt_until_host_boundary() {
        for (tail, parked, cap, attempted) in [
            (vec![], true, false, false),
            (vec![resume(false)], true, false, true),
            (
                vec![
                    resume(true),
                    Event::AcpSessionAssigned {
                        acp_session_id: "read-only".into(),
                        connection_ready: false,
                        native_subject: None,
                    },
                ],
                true,
                false,
                true,
            ),
            (
                vec![stopped(RATE_LIMIT_EXHAUSTED_RETRIES_REASON)],
                true,
                true,
                false,
            ),
            (vec![user_prompt("next")], false, false, false),
            (vec![Event::SessionCleared], false, false, false),
            (vec![stopped("user_stopped")], false, false, false),
            (
                vec![Event::AgentSwitched {
                    from: "claude".into(),
                    to: "codex".into(),
                    reason: "explicit".into(),
                }],
                false,
                false,
                false,
            ),
        ] {
            let (_tmp, store) = open_store(0);
            // Admission is stored SDK metadata, not Native establishment.
            record_projection(
                &store,
                "s",
                1,
                [Event::ThinkingStarted, rate_limit_event(60)],
            );
            record_projection(&store, "s", 3, tail);
            match store.rate_limit_park("s").unwrap() {
                Known(Some(park)) if parked => {
                    assert_eq!(park.cap_reached, cap);
                    assert!(park.info.is_some());
                    assert_eq!(park.last_resume_attempt_ms.is_some(), attempted);
                }
                Known(None) if !parked => {}
                actual => panic!("unexpected park: {actual:?}"),
            }
        }
        let (_tmp, store) = open_store(0);
        let cap = stopped(RATE_LIMIT_EXHAUSTED_RETRIES_REASON);
        store
            .record(
                "s",
                1,
                &cap,
                &crate::acp::event_sender::EventMetadata::host(&cap),
            )
            .unwrap();
        let Known(Some(park)) = store.rate_limit_park("s").unwrap() else {
            panic!("cap park");
        };
        assert!(park.cap_reached && park.info.is_none());
    }

    #[test]
    fn interrupted_input_preserves_attachments_and_diff_comments() {
        let attachment = crate::daemon::PromptAttachmentRef {
            id: "att-1".into(),
            kind: crate::daemon::PromptAttachmentKind::Image,
            mime_type: "image/png".into(),
            name: Some("shot.png".into()),
            size: 42,
        };
        for (input, text, attachments) in [
            (user_prompt("plain"), "plain", vec![]),
            (
                Event::UserPromptSent {
                    prompt_id: None,
                    text: "image".into(),
                    attachments: vec![attachment.clone()],
                    synthesized: false,
                    origin_plugin: None,
                },
                "image",
                vec![attachment],
            ),
            (
                Event::UserDiffCommentsPrompt {
                    intro: String::new(),
                    outro: String::new(),
                    is_multi_repo: false,
                    comments: vec![],
                    assembled_markdown: "review notes".into(),
                },
                "review notes",
                vec![],
            ),
        ] {
            let (_tmp, store) = open_store(0);
            record_projection(
                &store,
                "s",
                1,
                [
                    input,
                    Event::ThinkingStarted,
                    rate_limit_event(60),
                    stopped("rate_limited"),
                ],
            );
            assert_eq!(
                store.rate_limited_turn_prompt("s").unwrap(),
                Known(Some((text.to_owned(), attachments)))
            );
            assert_eq!(
                store.rate_limit_interrupted_agent_turn("s").unwrap(),
                Known(false)
            );
        }
    }

    #[test]
    fn latest_quota_is_result_option_and_preserves_recording_time() {
        let (_tmp, store) = open_store(0);
        let before = Utc::now().timestamp_millis();
        let latest = rate_limit_event(120);
        let Event::RateLimit { info: expected } = &latest else {
            unreachable!()
        };
        let expected_resets = expected.resets_at;
        record_projection(
            &store,
            "s",
            1,
            [
                Event::ThinkingStarted,
                rate_limit_event(60),
                stopped("rate_limited"),
                Event::ThinkingStarted,
                latest,
            ],
        );
        let after = Utc::now().timestamp_millis();
        let (info, at) = store
            .latest_rate_limit_event("s")
            .unwrap()
            .expect("stored quota");
        assert_eq!(info.resets_at, expected_resets);
        assert!((before..=after).contains(&at));
        assert!(store.latest_rate_limit_event("missing").unwrap().is_none());
    }

    #[test]
    fn interrupted_agent_requires_work_in_admitted_unprompted_operation() {
        for (prefix, work, expected) in [
            (vec![], true, true),
            (vec![], false, false),
            (vec![user_prompt("work")], true, false),
            (
                vec![
                    user_prompt("old"),
                    Event::ThinkingStarted,
                    stopped("prompt_complete"),
                    Event::SessionCleared,
                ],
                true,
                true,
            ),
            (
                vec![
                    Event::ThinkingStarted,
                    agent_chunk("old"),
                    stopped("agent_idle"),
                ],
                false,
                false,
            ),
        ] {
            let (_tmp, store) = open_store(0);
            let mut events = prefix;
            events.push(Event::ThinkingStarted);
            if work {
                events.push(agent_chunk("new"));
            }
            events.extend([rate_limit_event(0), stopped("rate_limited")]);
            record_projection(&store, "s", 1, events);
            assert_eq!(
                store.rate_limit_interrupted_agent_turn("s").unwrap(),
                Known(expected)
            );
        }
    }

    #[test]
    fn durable_budget_survives_retention_and_missing_budget_never_uses_history_arithmetic() {
        for cap in [0, 1, 3] {
            for durable in [None, Some((7, 1, 1)), Some((4, 1, 0))] {
                let (tmp, store) = open_store(cap);
                record_from(
                    &store,
                    "s",
                    1,
                    [
                        user_prompt("historical"),
                        resume(false),
                        user_prompt("historical redelivery"),
                        stopped("prompt_complete"),
                    ],
                );
                {
                    let conn = store.conn();
                    conn.execute(
                        "DELETE FROM acp_rate_limit_budgets WHERE session_id='s'",
                        [],
                    )
                    .unwrap();
                    if let Some((spent, armed, known)) = durable {
                        conn.execute(
                            "INSERT INTO acp_rate_limit_budgets VALUES('s',?1,?2,?3)",
                            params![spent, armed, known],
                        )
                        .unwrap();
                    }
                }
                store.control_cache.forget("s");
                let expected = match durable {
                    Some((spent, _, 1)) => Known(spent),
                    _ => Indeterminate,
                };
                assert_eq!(store.rate_limit_redelivery_streak("s").unwrap(), expected);
                drop(store);
                let cold = EventStore::open(&tmp.path().join("acp.db"), cap).unwrap();
                assert_eq!(cold.rate_limit_redelivery_streak("s").unwrap(), expected);
                // An explicit input neither seeds a lost budget nor spends a historical resume.
                let input = user_prompt("new explicit input");
                cold.record(
                    "s",
                    5,
                    &input,
                    &crate::acp::event_sender::EventMetadata::host(&input),
                )
                .unwrap();
                let expected = match durable {
                    Some((spent, 1, 1)) => Known(spent + 1),
                    _ => expected,
                };
                assert_eq!(cold.rate_limit_redelivery_streak("s").unwrap(), expected);
                record_projection(&cold, "s", 6, [Event::ThinkingStarted]);
                assert_eq!(cold.rate_limit_redelivery_streak("s").unwrap(), expected);
            }
        }
    }

    #[test]
    fn stored_sdk_retry_budget_arms_spends_disarms_and_resets() {
        for (tail, expected) in [
            (vec![resume(false)], 0),
            (vec![resume(false), user_prompt("retry")], 1),
            (vec![resume(true), user_prompt("manual retry")], 0),
            (
                vec![
                    resume(false),
                    Event::AgentStartupError {
                        message: "spawn failed".into(),
                        native_subject: None,
                    },
                    user_prompt("later explicit"),
                ],
                0,
            ),
            (
                vec![
                    resume(false),
                    user_prompt("retry"),
                    stopped("prompt_complete"),
                ],
                0,
            ),
            (
                vec![
                    resume(false),
                    user_prompt("retry"),
                    Event::AgentSwitched {
                        from: "claude".into(),
                        to: "codex".into(),
                        reason: "explicit".into(),
                    },
                ],
                0,
            ),
        ] {
            let (_tmp, store) = open_store(0);
            let mut events = vec![
                user_prompt("original"),
                Event::ThinkingStarted,
                rate_limit_event(0),
                stopped("rate_limited"),
            ];
            events.extend(tail);
            record_projection(&store, "s", 1, events);
            assert_eq!(
                store.rate_limit_redelivery_streak("s").unwrap(),
                Known(expected)
            );
        }
    }

    #[test]
    fn historical_controls_cannot_park_interrupt_or_rearm_budget() {
        let (_tmp, store) = open_store(0);
        record_from(
            &store,
            "s",
            1,
            [
                user_prompt("old"),
                Event::ThinkingStarted,
                agent_chunk("old work"),
                rate_limit_event(0),
                stopped("rate_limited"),
                resume(false),
            ],
        );
        assert!(matches!(store.rate_limit_park("s").unwrap(), Known(None)));
        assert_eq!(store.rate_limited_turn_prompt("s").unwrap(), Known(None));
        assert_eq!(
            store.rate_limit_interrupted_agent_turn("s").unwrap(),
            Known(false)
        );
        assert_eq!(store.rate_limit_redelivery_streak("s").unwrap(), Known(0));
        assert!(store.latest_rate_limit_event("s").unwrap().is_none());
    }

    #[test]
    fn quota_payload_and_projection_storage_errors_are_results() {
        let (_tmp, store) = open_store(0);
        record_projection(
            &store,
            "s",
            1,
            [Event::ThinkingStarted, rate_limit_event(0)],
        );
        store
            .conn()
            .execute(
                "UPDATE acp_events SET event_json='not json' WHERE session_id='s' AND seq=2",
                [],
            )
            .unwrap();
        // Keep the cached anchor, then require payload reads to report corruption.
        assert!(store.latest_rate_limit_event("s").is_err());
        assert!(store.rate_limit_park("s").is_err());
        store.control_cache.forget("s");
        assert!(store.rate_limit_redelivery_streak("s").is_err());
        assert!(store.rate_limited_turn_prompt("s").is_err());
        assert!(store.rate_limit_interrupted_agent_turn("s").is_err());
    }
}
