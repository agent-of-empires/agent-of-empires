//! Rate-limit parks and the redelivery budget that bounds auto-resume.

use rusqlite::{params, OptionalExtension};
use tracing::warn;

use super::{decode, logged, EventStore};
use crate::acp::state::{Event, RateLimitInfo, RATE_LIMIT_EXHAUSTED_RETRIES_REASON};
use crate::events;

/// A session parked on a provider rate limit.
#[derive(Debug, Clone)]
pub struct RateLimitPark {
    /// The latest reported limit; `None` for a park whose `RateLimit` row was pruned.
    pub info: Option<RateLimitInfo>,
    /// When the latest `RateLimit` was recorded (epoch ms), 0 when unknown.
    pub recorded_at_ms: i64,
    /// The redelivery cap already parked the session; only a queued prompt
    /// or a manual resume releases it.
    pub cap_reached: bool,
    /// When auto-resume last fired for this park, so a failed resume retries on a schedule.
    pub last_resume_attempt_ms: Option<i64>,
}

impl EventStore {
    /// The most recent `RateLimit` and the epoch ms it was recorded at.
    pub fn latest_rate_limit_event(&self, session_id: &str) -> Option<(RateLimitInfo, i64)> {
        let (json, created_at): (String, i64) = self
            .conn()
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
            .flatten()?;
        match decode(&json)? {
            Event::RateLimit { info } => Some((info, created_at)),
            _ => None,
        }
    }

    /// The session's current rate-limit park, unless a real continuation
    /// (a prompt, an agent switch, a new session, or an organic stop) superseded it.
    pub fn rate_limit_park(&self, session_id: &str) -> Option<RateLimitPark> {
        let conn = self.conn();
        let latest_rate_limit: Option<(i64, String, i64)> = conn
            .query_row(
                "SELECT seq, event_json, created_at FROM acp_events
                 WHERE session_id = ?1 AND discriminant = 'RateLimit'
                 ORDER BY seq DESC LIMIT 1",
                params![session_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .unwrap_or_else(|e| {
                warn!(target: "acp.event_store", "rate_limit_park query for {session_id}: {e}");
                None
            });
        let (anchor_seq, info, recorded_at_ms) = match latest_rate_limit {
            Some((seq, json, created_at)) => match decode(&json) {
                Some(Event::RateLimit { info }) => (seq, Some(info), created_at),
                _ => (seq, None, created_at),
            },
            // Retention may have pruned the `RateLimit` row while its stop survives.
            None => conn
                .query_row(
                    "SELECT seq, created_at FROM acp_events
                     WHERE session_id = ?1 AND discriminant = 'Stopped'
                       AND json_extract(event_json, '$.Stopped.reason') = 'rate_limited'
                     ORDER BY seq DESC LIMIT 1",
                    params![session_id],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()
                .unwrap_or(None)
                .map_or((0, None, 0), |(seq, created_at)| (seq, None, created_at)),
        };
        let cap_seq: Option<i64> = conn
            .query_row(
                "SELECT MAX(seq) FROM acp_events
                 WHERE session_id = ?1 AND seq > ?2 AND discriminant = 'Stopped'
                   AND json_extract(event_json, '$.Stopped.reason') = ?3",
                params![session_id, anchor_seq, RATE_LIMIT_EXHAUSTED_RETRIES_REASON],
                |row| row.get::<_, Option<i64>>(0),
            )
            .unwrap_or(None);
        let park_seq = match (cap_seq, anchor_seq) {
            (Some(cap), _) => cap,
            (None, 0) => return None,
            (None, anchor) => anchor,
        };
        let superseded: bool = conn
            .query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM acp_events
                   WHERE session_id = ?1 AND seq > ?2
                     AND (discriminant IN ('UserPromptSent', 'UserDiffCommentsPrompt',
                                           'AgentSwitched', 'AcpSessionAssigned')
                       OR (discriminant = 'Stopped'
                           AND json_extract(event_json, '$.Stopped.reason')
                               NOT IN ('rate_limited', ?3))))",
                params![session_id, park_seq, RATE_LIMIT_EXHAUSTED_RETRIES_REASON],
                |row| row.get::<_, bool>(0),
            )
            .unwrap_or(false);
        if superseded {
            return None;
        }
        let last_resume_attempt_ms: Option<i64> = conn
            .query_row(
                "SELECT MAX(created_at) FROM acp_events
                 WHERE session_id = ?1 AND seq > ?2
                   AND discriminant = 'RateLimitAutoResumed'",
                params![session_id, anchor_seq],
                |row| row.get::<_, Option<i64>>(0),
            )
            .unwrap_or(None);
        Some(RateLimitPark {
            info,
            recorded_at_ms,
            cap_reached: cap_seq.is_some(),
            last_resume_attempt_ms,
        })
    }

    /// The prompt a rate limit interrupted: the latest `UserPromptSent`
    /// whose next terminal event is `Stopped { reason: "rate_limited" }`.
    pub fn rate_limited_turn_prompt(
        &self,
        session_id: &str,
    ) -> Option<(String, Vec<crate::daemon::PromptAttachmentRef>)> {
        let conn = self.conn();
        let (prompt_seq, prompt_json): (i64, String) = logged(
            conn.query_row(
                "SELECT seq, event_json FROM acp_events
                 WHERE session_id = ?1
                   AND json_extract(event_json, '$.UserPromptSent') IS NOT NULL
                 ORDER BY seq DESC
                 LIMIT 1",
                params![session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional(),
            "rate_limited_turn_prompt prompt query",
            session_id,
        )
        .flatten()?;
        let Event::UserPromptSent {
            text, attachments, ..
        } = decode(&prompt_json)?
        else {
            return None;
        };
        let terminator: Option<String> = logged(
            conn.query_row(
                "SELECT event_json FROM acp_events
                 WHERE session_id = ?1
                   AND seq > ?2
                   AND (json_extract(event_json, '$.Stopped') IS NOT NULL
                     OR json_extract(event_json, '$.AgentStartupError') IS NOT NULL)
                 ORDER BY seq ASC
                 LIMIT 1",
                params![session_id, prompt_seq],
                |row| row.get(0),
            )
            .optional(),
            "rate_limited_turn_prompt terminator query",
            session_id,
        )
        .flatten();
        match terminator.and_then(|json| decode(&json)) {
            Some(Event::Stopped { reason }) if reason == "rate_limited" => {
                Some((text, attachments))
            }
            _ => None,
        }
    }

    /// Redeliveries in the current streak: automatic resumes that re-sent the
    /// interrupted prompt without a turn getting through. Read from the
    /// durable budget row, or derived from the log for sessions without one.
    pub fn rate_limit_redelivery_streak(&self, session_id: &str) -> i64 {
        let conn = self.conn();
        let durable: Option<i64> = logged(
            conn.query_row(
                &format!(
                    "SELECT spent FROM {} WHERE session_id = ?1",
                    self.schema.rate_limit_budgets_table()
                ),
                params![session_id],
                |row| row.get(0),
            )
            .optional(),
            "rate_limit budget row read",
            session_id,
        )
        .flatten();
        if let Some(spent) = durable {
            return spent;
        }
        logged(
            conn.query_row(
                &streak_sql("acp_events", false),
                params![session_id],
                |row| row.get::<_, i64>(0),
            ),
            "rate_limit_redelivery_streak",
            session_id,
        )
        .unwrap_or(0)
    }
}

/// The log derivation of the streak: automatic resume breadcrumbs after the
/// last organic boundary whose next prompt, startup error, or resume is the
/// redelivered `UserPromptSent`. `bounded` restricts it to rows below `?2`.
fn streak_sql(events_table: &str, bounded: bool) -> String {
    let below = if bounded { "AND r.seq < ?2" } else { "" };
    let next_below = if bounded { "AND n.seq < ?2" } else { "" };
    format!(
        "SELECT COUNT(*) FROM {events_table} r
         WHERE r.session_id = ?1
           AND r.discriminant = 'RateLimitAutoResumed'
           AND IFNULL(
                 json_extract(r.event_json, '$.RateLimitAutoResumed.manual'), 0) = 0
           {below}
           AND r.seq > (
               SELECT IFNULL(MAX(seq), 0) FROM {events_table}
               WHERE session_id = ?1
                 AND (discriminant = 'AgentSwitched'
                   OR (discriminant = 'Stopped'
                       AND json_extract(event_json, '$.Stopped.reason')
                           != 'rate_limited'))
           )
           AND IFNULL((
               SELECT n.discriminant FROM {events_table} n
               WHERE n.session_id = ?1
                 AND n.seq > r.seq
                 {next_below}
                 AND n.discriminant IN (
                       'UserPromptSent', 'AgentStartupError', 'RateLimitAutoResumed')
               ORDER BY n.seq ASC
               LIMIT 1
           ), '') = 'UserPromptSent'"
    )
}

/// Advance the durable redelivery budget for one newly inserted event,
/// following the same rules as the log derivation.
pub(super) fn update_rate_limit_budget(
    conn: &rusqlite::Transaction<'_>,
    schema: &events::Schema,
    session_id: &str,
    seq: u64,
    event: &Event,
) {
    enum Step {
        /// An automatic resume fired; its redelivery has not landed yet.
        Arm,
        /// The redelivered (or organic) prompt.
        Spend,
        /// A manual resume, or a spawn that burned no prompt: keep the spend, drop the arming.
        Disarm,
        /// An organic turn end or an agent switch ends the streak.
        Reset,
    }
    let step = match event {
        Event::RateLimitAutoResumed { manual: false, .. } => Step::Arm,
        Event::RateLimitAutoResumed { manual: true, .. } | Event::AgentStartupError { .. } => {
            Step::Disarm
        }
        Event::UserPromptSent { .. } => Step::Spend,
        Event::AgentSwitched { .. } => Step::Reset,
        Event::Stopped { reason } if reason != "rate_limited" => Step::Reset,
        _ => return,
    };
    let table = schema.rate_limit_budgets_table();
    let existing: Option<(i64, i64)> = logged(
        conn.query_row(
            &format!("SELECT spent, armed FROM {table} WHERE session_id = ?1"),
            params![session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional(),
        "budget row read",
        session_id,
    )
    .flatten();
    let (spent, armed) =
        existing.unwrap_or_else(|| seed_budget_from_log(conn, schema, session_id, seq));
    let (spent, armed) = match step {
        Step::Arm => (spent, 1),
        Step::Spend if armed == 1 => (spent + 1, 0),
        Step::Spend | Step::Disarm => (spent, 0),
        Step::Reset => (0, 0),
    };
    let wrote = conn
        .execute(
            &format!(
                "INSERT INTO {table} (session_id, spent, armed)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_id) DO UPDATE SET
                     spent = excluded.spent,
                     armed = excluded.armed"
            ),
            params![session_id, spent, armed],
        )
        .unwrap_or(0);
    if wrote == 0 {
        warn!(target: "acp.event_store", "budget row write {session_id}@{seq} failed");
    }
}

/// Derive `(spent, armed)` for a session without a budget row from the log
/// below `seq`.
fn seed_budget_from_log(
    conn: &rusqlite::Transaction<'_>,
    schema: &events::Schema,
    session_id: &str,
    seq: u64,
) -> (i64, i64) {
    let events_table = schema.events_table();
    let spent: i64 = conn
        .query_row(
            &streak_sql(events_table, true),
            params![session_id, seq as i64],
            |row| row.get(0),
        )
        .unwrap_or(0);
    // Armed iff the newest relevant event before `seq` is an automatic resume.
    let armed: i64 = conn
        .query_row(
            &format!(
                "SELECT IFNULL((
                    SELECT CASE WHEN d.discriminant = 'RateLimitAutoResumed'
                                AND IFNULL(json_extract(d.event_json,
                                    '$.RateLimitAutoResumed.manual'), 0) = 0
                            THEN 1 ELSE 0 END
                    FROM {events_table} d
                    WHERE d.session_id = ?1 AND d.seq < ?2
                      AND d.discriminant IN ('UserPromptSent', 'AgentStartupError',
                                             'RateLimitAutoResumed')
                    ORDER BY d.seq DESC LIMIT 1
                ), 0)"
            ),
            params![session_id, seq as i64],
            |row| row.get(0),
        )
        .unwrap_or(0);
    (spent, armed)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use chrono::Utc;

    fn rate_limit_event(secs_until_reset: i64) -> Event {
        Event::RateLimit {
            info: RateLimitInfo {
                status: "usage limit reached".into(),
                resets_at: Some(Utc::now() + chrono::Duration::seconds(secs_until_reset)),
                kind: "rate_limit".into(),
            },
        }
    }

    fn auto_resume() -> Event {
        Event::RateLimitAutoResumed {
            resets_at: Utc::now(),
            manual: false,
        }
    }

    #[test]
    fn latest_rate_limit_event_returns_most_recent_with_recorded_at() {
        let (_tmp, store) = open_store(1000);
        let before = Utc::now().timestamp_millis();
        store.record("s-1", 1, &rate_limit_event(3600)).unwrap();
        let second = rate_limit_event(7200);
        let Event::RateLimit { info: ref expected } = second else {
            unreachable!()
        };
        let expected_resets = expected.resets_at;
        store.record("s-1", 2, &second).unwrap();
        let after = Utc::now().timestamp_millis();

        let (info, recorded_at) = store.latest_rate_limit_event("s-1").expect("stored");
        assert_eq!(info.resets_at, expected_resets, "latest event wins");
        assert!((before..=after).contains(&recorded_at));
        assert!(store.latest_rate_limit_event("s-2").is_none());
    }

    #[test]
    fn rate_limited_turn_prompt_returns_only_the_interrupted_latest_prompt() {
        let (_tmp, store) = open_store(1000);
        let attachment = crate::daemon::PromptAttachmentRef {
            id: "att-1".into(),
            kind: crate::daemon::PromptAttachmentKind::Image,
            mime_type: "image/png".into(),
            name: Some("shot.png".into()),
            size: 42,
        };
        let with_attachment = Event::UserPromptSent {
            prompt_id: None,
            text: "look at this".into(),
            attachments: vec![attachment.clone()],
            synthesized: false,
        };
        // (events, expected prompt)
        let cases = [
            (
                vec![user_prompt("keep working"), stopped("rate_limited")],
                Some(("keep working", vec![])),
            ),
            (
                vec![with_attachment, stopped("rate_limited")],
                Some(("look at this", vec![attachment])),
            ),
            (vec![user_prompt("go"), stopped("prompt_complete")], None),
            // An agent-initiated turn hit the limit after a completed prompt.
            (
                vec![
                    user_prompt("old"),
                    stopped("prompt_complete"),
                    stopped("rate_limited"),
                ],
                None,
            ),
            (vec![], None),
        ];
        for (i, (events, want)) in cases.into_iter().enumerate() {
            let id = format!("s-{i}");
            record_from(&store, &id, 1, events);
            let want = want.map(|(text, attachments)| (text.to_string(), attachments));
            assert_eq!(store.rate_limited_turn_prompt(&id), want, "case {i}");
        }
    }

    #[test]
    fn rate_limit_park_survives_everything_but_a_real_continuation() {
        let (_tmp, store) = open_store(1000);
        let info = RateLimitInfo {
            status: "limited".into(),
            resets_at: None,
            kind: "usage".into(),
        };
        let cases: Vec<(&str, Vec<Event>, bool, bool)> = vec![
            ("bare park", vec![], true, false),
            (
                "failed resume",
                vec![
                    auto_resume(),
                    Event::AgentStartupError {
                        message: "spawn failed".into(),
                    },
                ],
                true,
                false,
            ),
            (
                "a second rate-limited stop",
                vec![stopped("rate_limited")],
                true,
                false,
            ),
            (
                "prompt got through",
                vec![user_prompt("again")],
                false,
                false,
            ),
            (
                "agent switched",
                vec![Event::AgentSwitched {
                    from: "claude".into(),
                    to: "codex".into(),
                    reason: "rate_limited".into(),
                }],
                false,
                false,
            ),
            (
                "resumed with nothing to redeliver",
                vec![
                    Event::RateLimitAutoResumed {
                        resets_at: Utc::now(),
                        manual: true,
                    },
                    Event::AcpSessionAssigned {
                        acp_session_id: "acp-2".into(),
                    },
                ],
                false,
                false,
            ),
            ("user stopped", vec![stopped("user_stopped")], false, false),
            (
                "cap reached",
                vec![stopped(RATE_LIMIT_EXHAUSTED_RETRIES_REASON)],
                true,
                true,
            ),
        ];
        for (i, (label, events, parked, cap)) in cases.into_iter().enumerate() {
            let id = format!("s-{i}");
            record_from(
                &store,
                &id,
                1,
                [
                    Event::RateLimit { info: info.clone() },
                    stopped("rate_limited"),
                ],
            );
            record_from(&store, &id, 3, events);
            let got = store.rate_limit_park(&id);
            assert_eq!(got.is_some(), parked, "{label}: parked");
            if let Some(got) = got {
                assert_eq!(got.cap_reached, cap, "{label}: cap");
                assert!(got.info.is_some(), "{label}: carries the limit");
                assert_eq!(
                    got.last_resume_attempt_ms.is_some(),
                    label == "failed resume",
                    "{label}: resume attempt"
                );
            }
        }

        // A cap park whose RateLimit row is gone still reads as a cap park.
        store
            .record(
                "s-cap-only",
                1,
                &stopped(RATE_LIMIT_EXHAUSTED_RETRIES_REASON),
            )
            .unwrap();
        let cap_only = store.rate_limit_park("s-cap-only").expect("cap park");
        assert!(cap_only.cap_reached && cap_only.info.is_none());
        assert!(store.rate_limit_park("never-limited").is_none());
    }

    #[test]
    fn rate_limit_redelivery_streak_counts_resumes_and_resets_organically() {
        let (_tmp, store) = open_store(1000);
        let manual_resume = || Event::RateLimitAutoResumed {
            resets_at: Utc::now(),
            manual: true,
        };
        let streak = || store.rate_limit_redelivery_streak("s-1");

        record_from(
            &store,
            "s-1",
            1,
            [
                user_prompt("run the nightly task"),
                rate_limit_event(0),
                stopped("rate_limited"),
            ],
        );
        for seq in 4..=18 {
            let ev = match seq % 3 {
                1 => auto_resume(),
                2 => user_prompt("run the nightly task"),
                _ => rate_limit_event(0),
            };
            store.record("s-1", seq, &ev).unwrap();
            if seq % 3 == 0 {
                store
                    .record("s-1", seq + 15, &stopped("rate_limited"))
                    .unwrap();
            }
        }
        assert_eq!(streak(), 5, "five park, resume, redeliver cycles");

        record_from(
            &store,
            "s-1",
            100,
            [
                stopped("prompt_complete"),
                user_prompt("run the nightly task"),
                rate_limit_event(0),
                stopped("rate_limited"),
                auto_resume(),
            ],
        );
        assert_eq!(streak(), 0, "an organic stop resets the streak");
        store
            .record("s-1", 105, &user_prompt("run the nightly task"))
            .unwrap();
        assert_eq!(streak(), 1);

        record_from(
            &store,
            "s-1",
            106,
            [
                rate_limit_event(0),
                stopped("rate_limited"),
                auto_resume(),
                Event::AgentStartupError {
                    message: "boom".into(),
                },
            ],
        );
        assert_eq!(streak(), 1, "a resume whose spawn failed spends nothing");
        record_from(
            &store,
            "s-1",
            110,
            [auto_resume(), user_prompt("run the nightly task")],
        );
        assert_eq!(streak(), 2);

        record_from(
            &store,
            "s-1",
            113,
            [
                rate_limit_event(0),
                stopped("rate_limited"),
                manual_resume(),
                user_prompt("run the nightly task"),
            ],
        );
        assert_eq!(streak(), 2, "a manual resume does not count");

        let switched = Event::AgentSwitched {
            from: "claude".into(),
            to: "codex".into(),
            reason: "rate_limit".into(),
        };
        store.record("s-1", 117, &switched).unwrap();
        assert_eq!(streak(), 0, "an agent switch resets the streak");
        record_from(
            &store,
            "s-1",
            118,
            [auto_resume(), user_prompt("run the nightly task")],
        );
        assert_eq!(streak(), 1);

        store.record("s-3", 1, &stopped("prompt_complete")).unwrap();
        for seq in 2..=16 {
            let ev = match seq % 3 {
                2 => rate_limit_event(0),
                0 => stopped("rate_limited"),
                _ => auto_resume(),
            };
            store.record("s-3", seq, &ev).unwrap();
        }
        assert_eq!(
            store.rate_limit_redelivery_streak("s-3"),
            0,
            "resumes that re-delivered nothing must not count toward the cap"
        );
        assert_eq!(store.rate_limit_redelivery_streak("s-2"), 0);
    }

    #[test]
    fn rate_limit_redelivery_streak_is_durable_under_small_retention() {
        for cap in [3, 1] {
            let (_tmp, store) = open_store(cap);
            store
                .record("s-1", 1, &user_prompt("run the nightly task"))
                .unwrap();
            for cycle in 0..5 {
                let base = 2 + cycle * 4;
                record_from(
                    &store,
                    "s-1",
                    base,
                    [
                        auto_resume(),
                        user_prompt("run the nightly task"),
                        rate_limit_event(0),
                    ],
                );
                store
                    .record("s-1", base + 17, &stopped("rate_limited"))
                    .unwrap();
            }
            assert_eq!(store.rate_limit_redelivery_streak("s-1"), 5, "cap {cap}");

            record_from(
                &store,
                "s-1",
                40,
                [stopped("prompt_complete"), auto_resume()],
            );
            assert_eq!(store.rate_limit_redelivery_streak("s-1"), 0, "cap {cap}");
            store
                .record("s-1", 42, &user_prompt("run the nightly task"))
                .unwrap();
            assert_eq!(store.rate_limit_redelivery_streak("s-1"), 1, "cap {cap}");
        }
    }

    #[test]
    fn rate_limit_redelivery_streak_seeds_from_log_for_upgraded_sessions() {
        let (_tmp, store) = open_store(1000);
        record_from(
            &store,
            "s-1",
            1,
            [
                user_prompt("run the nightly task"),
                rate_limit_event(0),
                stopped("rate_limited"),
                auto_resume(),
                user_prompt("run the nightly task"),
                rate_limit_event(0),
            ],
        );
        store.record("s-1", 21, &stopped("rate_limited")).unwrap();
        store
            .conn()
            .execute(
                &format!(
                    "DELETE FROM {} WHERE session_id = ?1",
                    store.schema.rate_limit_budgets_table()
                ),
                params!["s-1"],
            )
            .unwrap();
        assert_eq!(
            store.rate_limit_redelivery_streak("s-1"),
            1,
            "derived from the log"
        );
        store.record("s-1", 22, &auto_resume()).unwrap();
        assert_eq!(store.rate_limit_redelivery_streak("s-1"), 1);
        store
            .record("s-1", 23, &user_prompt("run the nightly task"))
            .unwrap();
        assert_eq!(store.rate_limit_redelivery_streak("s-1"), 2);
    }
}
