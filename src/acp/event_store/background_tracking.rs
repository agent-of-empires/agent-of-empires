use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

use super::{logged, EventStore};
use crate::acp::state::Event;
use crate::events::Schema;

/// Compact identities for exactly one native conversation, outside event retention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundAgentTrackingSeed {
    pub acp_session_id: String,
    pub agents: Vec<(String, bool)>,
}

impl EventStore {
    pub fn background_agent_tracking_seed(
        &self,
        session_id: &str,
    ) -> Option<BackgroundAgentTrackingSeed> {
        let conn = self.conn();
        let load = || -> rusqlite::Result<Option<BackgroundAgentTrackingSeed>> {
            let native_id = conn
                .query_row(
                    &format!(
                        "SELECT native_id FROM {} WHERE session_id = ?1",
                        self.schema.background_conversations_table()
                    ),
                    params![session_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            let Some(acp_session_id) = native_id else {
                return Ok(None);
            };
            let mut statement = conn.prepare(&format!(
                "SELECT agent_id, terminal FROM {} WHERE session_id = ?1
                 ORDER BY terminal DESC, CASE WHEN terminal = 0 THEN first_seq ELSE 0 END, agent_id",
                self.schema.background_agent_identities_table()
            ))?;
            let agents = statement
                .query_map(params![session_id], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(Some(BackgroundAgentTrackingSeed {
                acp_session_id,
                agents,
            }))
        };
        logged(load(), "background_agent_tracking_seed", session_id).flatten()
    }
}

pub(super) fn update(
    conn: &Connection,
    schema: &Schema,
    session_id: &str,
    seq: u64,
    event: &Event,
) -> Result<()> {
    let conversations = schema.background_conversations_table();
    let identities = schema.background_agent_identities_table();
    let seq = i64::try_from(seq)?;
    if let Event::AcpSessionAssigned { acp_session_id } = event {
        let previous: Option<(String, i64)> = conn
            .query_row(
                &format!(
                    "SELECT native_id, assigned_seq FROM {conversations} WHERE session_id = ?1"
                ),
                params![session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if previous
            .as_ref()
            .is_some_and(|(_, assigned)| *assigned >= seq)
        {
            return Ok(());
        }
        if previous.as_ref().is_none_or(|(id, _)| id != acp_session_id) {
            conn.execute(
                &format!("DELETE FROM {identities} WHERE session_id = ?1"),
                params![session_id],
            )?;
            conn.execute(
                &format!(
                    "INSERT INTO {conversations} (session_id, native_id, epoch_seq, assigned_seq)
                          VALUES (?1, ?2, ?3, ?3)
                          ON CONFLICT(session_id) DO UPDATE SET native_id = excluded.native_id,
                          epoch_seq = excluded.epoch_seq, assigned_seq = excluded.assigned_seq"
                ),
                params![session_id, acp_session_id, seq],
            )?;
        } else {
            conn.execute(
                &format!("UPDATE {conversations} SET assigned_seq = ?2 WHERE session_id = ?1"),
                params![session_id, seq],
            )?;
        }
        return Ok(());
    }
    match event {
        Event::BackgroundAgentLaunched {
            agent_id,
            wire_tracked: true,
            ..
        } => {
            conn.execute(
                &format!(
                    "INSERT INTO {identities} (session_id, agent_id, terminal, first_seq)
                     SELECT ?1, ?2, 0, ?3 FROM {conversations}
                     WHERE session_id = ?1 AND epoch_seq <= ?3
                     ON CONFLICT(session_id, agent_id) DO UPDATE SET
                     first_seq = MIN(first_seq, excluded.first_seq)"
                ),
                params![session_id, agent_id, seq],
            )?;
        }
        Event::BackgroundAgentCompleted { agent_id, .. } => {
            conn.execute(
                &format!(
                    "UPDATE {identities} SET terminal = 1
                     WHERE session_id = ?1 AND agent_id = ?2 AND first_seq <= ?3"
                ),
                params![session_id, agent_id, seq],
            )?;
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::state::BackgroundAgentStatus;

    fn assigned(id: &str) -> Event {
        Event::AcpSessionAssigned {
            acp_session_id: id.into(),
        }
    }

    fn launched(id: &str) -> Event {
        Event::BackgroundAgentLaunched {
            agent_id: id.into(),
            tool_call_id: format!("tool-{id}"),
            description: "child".into(),
            prompt: "work".into(),
            model: "test".into(),
            wire_tracked: true,
            output_file: String::new(),
            started_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        }
    }

    fn completed(id: &str, status: BackgroundAgentStatus) -> Event {
        Event::BackgroundAgentCompleted {
            agent_id: id.into(),
            status,
            tools: Vec::new(),
            result: None,
            warning: None,
            ended_at: chrono::DateTime::from_timestamp(1_700_000_001, 0).unwrap(),
        }
    }

    #[test]
    fn background_tracking_survives_real_retention_sweep_and_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("events.db");
        let store = EventStore::open(&path, 3).unwrap();
        store.record("s", 1, &assigned("native")).unwrap();
        store.record("s", 2, &launched("done")).unwrap();
        store
            .record("s", 3, &completed("done", BackgroundAgentStatus::Error))
            .unwrap();
        store
            .record("s", 4, &completed("done", BackgroundAgentStatus::Completed))
            .unwrap();
        store.record("s", 5, &launched("running")).unwrap();
        for seq in 6..=128 {
            store
                .record(
                    "s",
                    seq,
                    &Event::AgentMessageChunk {
                        text: "x".into(),
                        message_id: None,
                    },
                )
                .unwrap();
        }
        let retained_completed: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM acp_events WHERE discriminant = 'BackgroundAgentCompleted'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            retained_completed, 0,
            "exercise the completed-work retention sweep"
        );
        drop(store);
        let store = EventStore::open(&path, 3).unwrap();
        assert_eq!(
            store.background_agent_tracking_seed("s"),
            Some(BackgroundAgentTrackingSeed {
                acp_session_id: "native".into(),
                agents: vec![("done".into(), true), ("running".into(), false)],
            })
        );
        store.record("s", 129, &assigned("native")).unwrap();
        assert_eq!(
            store
                .background_agent_tracking_seed("s")
                .unwrap()
                .agents
                .len(),
            2
        );
        store.record("s", 130, &assigned("other")).unwrap();
        store.record("s", 131, &assigned("native")).unwrap();
        assert!(store
            .background_agent_tracking_seed("s")
            .unwrap()
            .agents
            .is_empty());
    }

    #[test]
    fn background_tracking_duplicate_inserts_and_projection_failures_are_atomic() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStore::open(&tmp.path().join("events.db"), 0).unwrap();
        store
            .record("unbound", 1, &launched("unbound-child"))
            .unwrap();
        store
            .record(
                "unbound",
                2,
                &completed("unbound-child", BackgroundAgentStatus::Completed),
            )
            .unwrap();
        assert!(store.background_agent_tracking_seed("unbound").is_none());
        store.record("s", 1, &assigned("native")).unwrap();
        store.record("s", 2, &launched("child")).unwrap();
        store
            .record(
                "s",
                2,
                &completed("child", BackgroundAgentStatus::Completed),
            )
            .unwrap();
        store.record("s", 1, &assigned("wrong-native")).unwrap();
        let seed = store.background_agent_tracking_seed("s").unwrap();
        assert_eq!(seed.acp_session_id, "native");
        assert_eq!(seed.agents, vec![("child".into(), false)]);
        store
            .conn()
            .execute_batch(
                "CREATE TRIGGER reject_identity BEFORE UPDATE ON acp_background_agent_identities
             BEGIN SELECT RAISE(ABORT, 'identity persistence failed'); END;",
            )
            .unwrap();
        assert!(store
            .record(
                "s",
                3,
                &completed("child", BackgroundAgentStatus::Completed)
            )
            .is_err());
        assert_eq!(store.highest_seq("s"), 2);
        assert_eq!(store.background_agent_tracking_seed("s").unwrap(), seed);
        store
            .conn()
            .execute_batch("DROP TRIGGER reject_identity")
            .unwrap();
        store
            .record("s", 3, &completed("child", BackgroundAgentStatus::Error))
            .unwrap();
        store
            .record(
                "s",
                4,
                &completed("child", BackgroundAgentStatus::Completed),
            )
            .unwrap();
        store.record("s", 5, &launched("child")).unwrap();
        assert_eq!(
            store.background_agent_tracking_seed("s").unwrap().agents,
            vec![("child".into(), true)]
        );
    }

    #[test]
    fn background_tracking_requires_current_wire_launch_provenance() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStore::open(&tmp.path().join("events.db"), 0).unwrap();
        store.record("s", 1, &assigned("old")).unwrap();
        store.record("s", 2, &launched("previous-epoch")).unwrap();
        store.record("s", 3, &assigned("native")).unwrap();
        let mut transcript = launched("transcript");
        if let Event::BackgroundAgentLaunched {
            wire_tracked,
            output_file,
            ..
        } = &mut transcript
        {
            *wire_tracked = false;
            *output_file = "/unused/transcript.jsonl".into();
        }
        store.record("s", 4, &transcript).unwrap();
        for (seq, id) in [(5, "transcript"), (6, "orphan"), (7, "previous-epoch")] {
            store
                .record("s", seq, &completed(id, BackgroundAgentStatus::Completed))
                .unwrap();
        }
        assert!(store
            .background_agent_tracking_seed("s")
            .unwrap()
            .agents
            .is_empty());
        store.record("s", 8, &launched("orphan")).unwrap();
        assert_eq!(
            store.background_agent_tracking_seed("s").unwrap().agents,
            vec![("orphan".into(), false)]
        );
        store
            .record(
                "s",
                9,
                &completed("orphan", BackgroundAgentStatus::Completed),
            )
            .unwrap();
        assert_eq!(
            store.background_agent_tracking_seed("s").unwrap().agents,
            vec![("orphan".into(), true)]
        );
    }

    #[test]
    fn background_tracking_session_deletion_clears_projection() {
        let tmp = tempfile::tempdir().unwrap();
        let store = EventStore::open(&tmp.path().join("events.db"), 0).unwrap();
        for session in ["checked", "topic", "keep"] {
            store.record(session, 1, &assigned("native")).unwrap();
            store.record(session, 2, &launched("child")).unwrap();
        }
        store.delete_session_checked("checked").unwrap();
        store.delete_session("topic");
        for session in ["checked", "topic"] {
            assert!(store.background_agent_tracking_seed(session).is_none());
            store.record(session, 1, &assigned("native")).unwrap();
            assert!(store
                .background_agent_tracking_seed(session)
                .unwrap()
                .agents
                .is_empty());
        }
        assert_eq!(
            store.background_agent_tracking_seed("keep").unwrap().agents,
            vec![("child".into(), false)]
        );
    }
}
