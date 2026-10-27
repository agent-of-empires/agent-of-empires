use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;
use tracing::info;

pub fn run() -> Result<()> {
    run_in(&crate::session::get_app_dir()?)
}

fn run_in(app_dir: &Path) -> Result<()> {
    let path = app_dir.join("acp_events.db");
    if !path.exists() {
        return Ok(());
    }
    let mut conn = Connection::open(&path).context("open event database for v051")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let has_events: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'acp_events')",
        [],
        |row| row.get(0),
    )?;
    if !has_events {
        return Ok(());
    }
    crate::events::ensure_discriminant_column(&conn, "acp_events")?;
    let tx = conn.transaction()?;
    crate::events::create_background_tracking_schema(&tx, &crate::events::Schema::new("acp")?)?;
    // Only retained identities are recoverable. Existing projections must never be replayed.
    tx.execute_batch(
        "CREATE TEMP TABLE background_tracking_backfill AS
         WITH assignments AS (
             SELECT session_id, seq,
                    json_extract(event_json, '$.AcpSessionAssigned.acp_session_id') AS native_id,
                    LAG(json_extract(event_json, '$.AcpSessionAssigned.acp_session_id'))
                        OVER (PARTITION BY session_id ORDER BY seq) AS previous_id
             FROM acp_events
             WHERE discriminant = 'AcpSessionAssigned'
         ), epochs AS (
             SELECT session_id, MAX(CASE WHEN previous_id IS NULL OR native_id != previous_id
                                        THEN seq END) AS epoch_seq,
                    MAX(seq) AS assigned_seq
             FROM assignments WHERE native_id IS NOT NULL GROUP BY session_id
         )
         SELECT epochs.session_id, assignments.native_id, epochs.epoch_seq, epochs.assigned_seq
         FROM epochs JOIN assignments ON assignments.session_id = epochs.session_id
                                     AND assignments.seq = epochs.assigned_seq
         WHERE NOT EXISTS (SELECT 1 FROM acp_background_conversations AS current
                           WHERE current.session_id = epochs.session_id);
         INSERT INTO acp_background_conversations (session_id, native_id, epoch_seq, assigned_seq)
         SELECT session_id, native_id, epoch_seq, assigned_seq FROM background_tracking_backfill;
         INSERT INTO acp_background_agent_identities (session_id, agent_id, terminal, first_seq)
         SELECT session_id, agent_id, MAX(terminal), MIN(seq) FROM (
             SELECT events.session_id, events.seq,
                    CASE events.discriminant
                        WHEN 'BackgroundAgentCompleted' THEN
                            json_extract(event_json, '$.BackgroundAgentCompleted.agent_id')
                        ELSE json_extract(event_json, '$.BackgroundAgentLaunched.agent_id') END AS agent_id,
                    events.discriminant = 'BackgroundAgentCompleted' AS terminal
             FROM acp_events AS events JOIN background_tracking_backfill AS current
                  ON current.session_id = events.session_id AND events.seq >= current.epoch_seq
             WHERE events.discriminant = 'BackgroundAgentCompleted'
                OR (events.discriminant = 'BackgroundAgentLaunched'
                    AND json_extract(event_json, '$.BackgroundAgentLaunched.wire_tracked') = 1)
         ) WHERE agent_id IS NOT NULL GROUP BY session_id, agent_id;
         DROP TABLE background_tracking_backfill;",
    )?;
    tx.commit()?;
    info!("v051: preserved retained current-conversation background identities");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_backfills_v012_schema_without_discriminant_and_preserves_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("acp_events.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE acp_events (
                session_id TEXT NOT NULL, seq INTEGER NOT NULL,
                event_json TEXT NOT NULL, created_at INTEGER NOT NULL,
                PRIMARY KEY (session_id, seq));
             CREATE TABLE acp_attachments (
                session_id TEXT NOT NULL, seq INTEGER NOT NULL, attachment_id TEXT NOT NULL,
                kind TEXT NOT NULL, mime_type TEXT NOT NULL, name TEXT,
                data BLOB NOT NULL, created_at INTEGER NOT NULL,
                PRIMARY KEY (session_id, attachment_id));
             INSERT INTO acp_attachments VALUES
                ('s', 7, 'attachment', 'image', 'image/png', 'retained.png', X'000102FF', 107);",
        )
        .unwrap();
        let events = [
            (
                "AcpSessionAssigned",
                r#"{"AcpSessionAssigned":{"acp_session_id":"old"}}"#,
            ),
            (
                "BackgroundAgentCompleted",
                r#"{"BackgroundAgentCompleted":{"agent_id":"old-child"}}"#,
            ),
            (
                "AcpSessionAssigned",
                r#"{"AcpSessionAssigned":{"acp_session_id":"current"}}"#,
            ),
            (
                "BackgroundAgentLaunched",
                r#"{"BackgroundAgentLaunched":{"agent_id":"running","wire_tracked":true}}"#,
            ),
            (
                "BackgroundAgentLaunched",
                r#"{"BackgroundAgentLaunched":{"agent_id":"finished","wire_tracked":true}}"#,
            ),
            (
                "BackgroundAgentCompleted",
                r#"{"BackgroundAgentCompleted":{"agent_id":"finished"}}"#,
            ),
            ("ThinkingStarted", r#""ThinkingStarted""#),
        ];
        for (index, (_, json)) in events.iter().enumerate() {
            let seq = index as i64 + 1;
            conn.execute(
                "INSERT INTO acp_events (session_id, seq, event_json, created_at)
                 VALUES ('s', ?1, ?2, ?3)",
                rusqlite::params![seq, json, 100 + seq],
            )
            .unwrap();
        }
        let read_events = || {
            conn.prepare(
                "SELECT session_id, seq, event_json, created_at
                 FROM acp_events ORDER BY session_id, seq",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
        };
        let retained_events = read_events();
        for _ in 0..2 {
            run_in(tmp.path()).unwrap();
            assert_eq!(read_events(), retained_events);
            let discriminants = conn
                .prepare("SELECT discriminant FROM acp_events ORDER BY seq")
                .unwrap()
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(discriminants, events.map(|(discriminant, _)| discriminant));
            let conversation = conn
                .query_row(
                    "SELECT native_id, epoch_seq, assigned_seq FROM acp_background_conversations
                     WHERE session_id = 's'",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    },
                )
                .unwrap();
            assert_eq!(conversation, ("current".into(), 3, 3));
            let identities = conn
                .prepare(
                    "SELECT session_id, agent_id, terminal, first_seq
                     FROM acp_background_agent_identities ORDER BY agent_id",
                )
                .unwrap()
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, bool>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(
                identities,
                vec![
                    ("s".into(), "finished".into(), true, 5),
                    ("s".into(), "running".into(), false, 4),
                ]
            );
            let attachment = conn
                .query_row(
                    "SELECT session_id, seq, attachment_id, kind, mime_type, name, data, created_at
                     FROM acp_attachments",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, Vec<u8>>(6)?,
                            row.get::<_, i64>(7)?,
                        ))
                    },
                )
                .unwrap();
            assert_eq!(
                attachment,
                (
                    "s".into(),
                    7,
                    "attachment".into(),
                    "image".into(),
                    "image/png".into(),
                    "retained.png".into(),
                    vec![0, 1, 2, 255],
                    107,
                )
            );
        }
    }

    #[test]
    fn migration_backfills_current_epoch_once_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("acp_events.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE acp_events (session_id TEXT, seq INTEGER, event_json TEXT, discriminant TEXT);
             INSERT INTO acp_events VALUES
             ('s', 1, '{\"AcpSessionAssigned\":{\"acp_session_id\":\"old\"}}', 'AcpSessionAssigned'),
             ('s', 2, '{\"BackgroundAgentCompleted\":{\"agent_id\":\"old-child\"}}', 'BackgroundAgentCompleted'),
             ('s', 3, '{\"AcpSessionAssigned\":{\"acp_session_id\":\"new\"}}', 'AcpSessionAssigned'),
             ('s', 4, '{\"BackgroundAgentCompleted\":{\"agent_id\":\"completion-only\"}}', 'BackgroundAgentCompleted'),
             ('s', 5, '{\"BackgroundAgentLaunched\":{\"agent_id\":\"running\",\"wire_tracked\":true}}', 'BackgroundAgentLaunched'),
             ('s', 6, '{\"AcpSessionAssigned\":{\"acp_session_id\":\"new\"}}', 'AcpSessionAssigned'),
             ('reused', 1, '{\"AcpSessionAssigned\":{\"acp_session_id\":\"first\"}}', 'AcpSessionAssigned'),
             ('reused', 2, '{\"BackgroundAgentCompleted\":{\"agent_id\":\"past-first\"}}', 'BackgroundAgentCompleted'),
             ('reused', 3, '{\"AcpSessionAssigned\":{\"acp_session_id\":\"second\"}}', 'AcpSessionAssigned'),
             ('reused', 4, '{\"BackgroundAgentCompleted\":{\"agent_id\":\"past-second\"}}', 'BackgroundAgentCompleted'),
             ('reused', 5, '{\"AcpSessionAssigned\":{\"acp_session_id\":\"first\"}}', 'AcpSessionAssigned');"
        ).unwrap();
        drop(conn);
        run_in(tmp.path()).unwrap();
        let conn = Connection::open(&path).unwrap();
        let read = || {
            conn.prepare(
                "SELECT agent_id, terminal FROM acp_background_agent_identities ORDER BY agent_id",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
        };
        assert_eq!(
            read(),
            vec![("completion-only".into(), true), ("running".into(), false)]
        );
        let native: String = conn
            .query_row(
                "SELECT native_id FROM acp_background_conversations WHERE session_id = 's'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(native, "new");
        conn.execute("DELETE FROM acp_events WHERE seq = 4", [])
            .unwrap();
        conn.execute("INSERT INTO acp_events VALUES ('s', 7, '{\"BackgroundAgentCompleted\":{\"agent_id\":\"not-replayed\"}}', 'BackgroundAgentCompleted')", []).unwrap();
        run_in(tmp.path()).unwrap();
        assert_eq!(
            read(),
            vec![("completion-only".into(), true), ("running".into(), false)]
        );
    }
}
