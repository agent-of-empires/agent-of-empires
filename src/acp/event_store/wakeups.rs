//! Scheduled wakeups and armed monitors.

use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use tracing::{debug, trace, warn};

use super::EventStore;
use crate::acp::state::Event;
use crate::events;

impl EventStore {
    /// The latest `WakeupScheduled`, while its `at` is still in the future.
    pub fn latest_pending_wakeup(
        &self,
        session_id: &str,
    ) -> Option<(DateTime<Utc>, Option<String>)> {
        let (_, json) = events::latest_by_discriminant(
            &self.conn(),
            &self.schema,
            session_id,
            "WakeupScheduled",
        )?;
        let Event::WakeupScheduled { at, reason } =
            decode_logged(&json, session_id, "latest_pending_wakeup")?
        else {
            return None;
        };
        let now = Utc::now();
        if at <= now {
            return None;
        }
        trace!(
            target: "acp.event_store",
            session = %session_id,
            wake_at = %at,
            in_secs = (at - now).num_seconds(),
            "latest_pending_wakeup: still pending"
        );
        Some((at, reason))
    }

    /// The description of an armed `Monitor`, until the user takes over or
    /// the work it fired ends its turn.
    pub fn latest_active_monitor(&self, session_id: &str) -> Option<Option<String>> {
        let conn = self.conn();
        let (armed_seq, json) =
            events::latest_by_discriminant(&conn, &self.schema, session_id, "MonitorArmed")?;
        let exists_after = |sql: &str, seq: i64| -> Option<i64> {
            conn.query_row(sql, params![session_id, seq], |row| row.get(0))
                .optional()
                .ok()
                .flatten()
        };
        let armed_seq = armed_seq as i64;
        let user_took_over = exists_after(
            "SELECT 1 FROM acp_events
             WHERE session_id = ?1
               AND seq > ?2
               AND discriminant = 'UserPromptSent'
             LIMIT 1",
            armed_seq,
        );
        if user_took_over.is_some() {
            return None;
        }
        let first_work_seq = exists_after(
            "SELECT MIN(seq) FROM acp_events
             WHERE session_id = ?1
               AND seq > ?2
               AND discriminant = 'ToolCallStarted'",
            armed_seq,
        );
        let turn_ended = first_work_seq.and_then(|work_seq| {
            exists_after(
                "SELECT 1 FROM acp_events
                 WHERE session_id = ?1
                   AND seq > ?2
                   AND discriminant = 'Stopped'
                 LIMIT 1",
                work_seq,
            )
        });
        if turn_ended.is_some() {
            return None;
        }
        match decode_logged(&json, session_id, "latest_active_monitor")? {
            Event::MonitorArmed { description } => Some(description),
            _ => None,
        }
    }

    /// The `WakeupScheduled` whose timer fired the just-published prompt at
    /// `prompt_seq`: its `at` passed before the prompt arrived and no earlier
    /// prompt already claimed it.
    pub fn fired_wakeup_for_prompt(
        &self,
        session_id: &str,
        prompt_seq: u64,
    ) -> Option<(DateTime<Utc>, Option<String>)> {
        let conn = self.conn();
        let prompt_seq_i64 = prompt_seq as i64;
        let Some(prompt_created_ms) = conn
            .query_row(
                "SELECT created_at FROM acp_events
                 WHERE session_id = ?1 AND seq = ?2",
                params![session_id, prompt_seq_i64],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .ok()
            .flatten()
        else {
            trace!(
                target: "acp.event_store",
                session = %session_id,
                seq = prompt_seq,
                "fired_wakeup_for_prompt: prompt row missing"
            );
            return None;
        };
        let Some((wake_seq, wake_json)) = conn
            .query_row(
                "SELECT seq, event_json FROM acp_events
                 WHERE session_id = ?1
                   AND seq < ?2
                   AND discriminant = 'WakeupScheduled'
                 ORDER BY seq DESC LIMIT 1",
                params![session_id, prompt_seq_i64],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .ok()
            .flatten()
        else {
            trace!(
                target: "acp.event_store",
                session = %session_id,
                prompt_seq,
                "fired_wakeup_for_prompt: no prior WakeupScheduled"
            );
            return None;
        };
        let Event::WakeupScheduled { at, reason } =
            decode_logged(&wake_json, session_id, "fired_wakeup_for_prompt")?
        else {
            return None;
        };
        let at_ms = at.timestamp_millis();
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
}

fn decode_logged(json: &str, session_id: &str, what: &str) -> Option<Event> {
    serde_json::from_str(json)
        .map_err(|e| {
            warn!(
                target: "acp.event_store",
                session = %session_id,
                "{what}: deserialise failed: {e}"
            )
        })
        .ok()
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;

    fn wakeup(secs_from_now: i64, reason: Option<&str>) -> Event {
        Event::WakeupScheduled {
            at: Utc::now() + chrono::Duration::seconds(secs_from_now),
            reason: reason.map(Into::into),
        }
    }

    #[test]
    fn pending_and_fired_wakeups_follow_the_latest_schedule() {
        let (_tmp, store) = open_store(1000);
        record_from(
            &store,
            "s-1",
            1,
            [
                user_prompt("schedule a wake in 2m"),
                wakeup(60, Some("first schedule")),
                wakeup(120, Some("rescheduled")),
                user_prompt("btw, ping me when you wake"),
            ],
        );
        let (at, reason) = store.latest_pending_wakeup("s-1").expect("still pending");
        assert!((at - Utc::now()).num_seconds() > 60);
        assert_eq!(reason.as_deref(), Some("rescheduled"));

        store.record("s-2", 1, &wakeup(-30, None)).unwrap();
        assert!(store.latest_pending_wakeup("s-2").is_none());

        // A fired wakeup is claimed once, by the first prompt past its time.
        let (_tmp, store) = open_store(1000);
        record_from(
            &store,
            "future",
            1,
            [
                user_prompt("schedule a wake"),
                wakeup(300, Some("test wake")),
                user_prompt("ping me when you wake"),
            ],
        );
        assert!(
            store.fired_wakeup_for_prompt("future", 3).is_none(),
            "a mid-wait follow-up is not a wake-fire"
        );

        record_from(
            &store,
            "past",
            1,
            [
                wakeup(-60, Some("test wake")),
                user_prompt("first prompt past at"),
                user_prompt("second prompt past at"),
            ],
        );
        let fired = store.fired_wakeup_for_prompt("past", 2).expect("wake-fire");
        assert_eq!(fired.1.as_deref(), Some("test wake"));
        assert!(
            store.fired_wakeup_for_prompt("past", 3).is_none(),
            "a later prompt must not claim the wake again"
        );
    }

    #[test]
    fn active_monitor_clears_on_takeover_or_after_fired_work_ends() {
        let (_tmp, store) = open_store(1000);
        let armed = Event::MonitorArmed {
            description: Some("watch".into()),
        };
        let work = Event::ToolCallStarted {
            tool_call: tool_call("tc-1"),
        };
        // (events after arming, still active)
        let cases = [
            (vec![], true),
            (vec![Event::ThinkingStarted, agent_chunk("resuming")], true),
            (vec![stopped("prompt_complete")], true),
            (vec![work.clone()], true),
            (vec![user_prompt("stop watching")], false),
            (vec![work.clone(), stopped("prompt_complete")], false),
            (vec![work, stopped("agent_idle")], false),
        ];
        for (i, (events, active)) in cases.into_iter().enumerate() {
            let id = format!("s-{i}");
            store.record(&id, 1, &armed).unwrap();
            record_from(&store, &id, 2, events);
            let got = store.latest_active_monitor(&id);
            assert_eq!(got.is_some(), active, "case {i}");
            if active {
                assert_eq!(got.unwrap().as_deref(), Some("watch"));
            }
        }
        store.record("none", 1, &user_prompt("hi")).unwrap();
        assert!(store.latest_active_monitor("none").is_none());
    }
}
