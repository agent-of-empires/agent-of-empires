use anyhow::Result;
use rusqlite::Connection;

pub(crate) fn install(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let installed: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'acp_history_store')",
        [],
        |row| row.get(0),
    )?;
    #[cfg(test)]
    tests::AFTER_READ.with(|hook| hook.take().map(|run| run()));
    if installed {
        tx.commit()?;
        return Ok(());
    }
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS acp_history_store (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            incarnation TEXT NOT NULL
        );
        INSERT OR IGNORE INTO acp_history_store VALUES (1, lower(hex(randomblob(16))));
        CREATE TABLE IF NOT EXISTS acp_history_versions (
            session_id TEXT PRIMARY KEY,
            version TEXT NOT NULL
        );
        INSERT OR IGNORE INTO acp_history_versions
            SELECT session_id, lower(hex(randomblob(16))) FROM acp_events GROUP BY session_id;
        CREATE TRIGGER IF NOT EXISTS acp_history_insert AFTER INSERT ON acp_events BEGIN
            INSERT INTO acp_history_versions VALUES (NEW.session_id, lower(hex(randomblob(16))))
                ON CONFLICT(session_id) DO UPDATE SET version = excluded.version;
        END;
        CREATE TRIGGER IF NOT EXISTS acp_history_delete AFTER DELETE ON acp_events BEGIN
            INSERT INTO acp_history_versions VALUES (OLD.session_id, lower(hex(randomblob(16))))
                ON CONFLICT(session_id) DO UPDATE SET version = excluded.version;
        END;
        CREATE TRIGGER IF NOT EXISTS acp_history_update AFTER UPDATE ON acp_events
            WHEN OLD.session_id IS NOT NEW.session_id
                OR OLD.seq IS NOT NEW.seq OR OLD.event_json IS NOT NEW.event_json
        BEGIN
            INSERT INTO acp_history_versions
                SELECT OLD.session_id, lower(hex(randomblob(16)))
                WHERE OLD.session_id IS NOT NEW.session_id
                ON CONFLICT(session_id) DO UPDATE SET version = excluded.version;
            INSERT INTO acp_history_versions VALUES (NEW.session_id, lower(hex(randomblob(16))))
                ON CONFLICT(session_id) DO UPDATE SET version = excluded.version;
        END;",
    )?;
    tx.commit()?;
    Ok(())
}

pub(super) fn read(conn: &Connection, session_id: &str) -> Result<String> {
    Ok(conn.query_row(
        "SELECT s.incarnation || ':' || COALESCE(v.version, 'empty')
         FROM acp_history_store s LEFT JOIN acp_history_versions v ON v.session_id = ?1
         WHERE s.id = 1",
        [session_id],
        |row| row.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::{event_store::EventStore, state::Event};

    thread_local! {
        pub(super) static AFTER_READ: std::cell::Cell<Option<Box<dyn FnOnce()>>> =
            const { std::cell::Cell::new(None) };
    }

    #[test]
    fn concurrent_installers_preserve_identity_and_track_later_content() -> Result<()> {
        use std::cell::RefCell;
        use std::sync::mpsc::{self, Receiver, Sender};
        use std::time::Duration;
        #[derive(Debug)]
        enum Progress {
            Contended,
            Finished,
        }
        thread_local! {
            static BUSY: RefCell<Option<(Sender<Progress>, Receiver<()>)>> =
                const { RefCell::new(None) };
        }
        fn busy(_: i32) -> bool {
            BUSY.with(|context| {
                let context = context.borrow();
                let (events, release) = context.as_ref().unwrap();
                let _ = events.send(Progress::Contended);
                release.recv_timeout(Duration::from_secs(10)).is_ok()
            })
        }
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("legacy.db");
        let schema = crate::events::Schema::new("acp")?;
        drop(crate::events::open(&path, &schema)?);
        let (ready, arrived) = mpsc::channel();
        let (release_first, wait_first) = mpsc::channel();
        let first_path = path.clone();
        let first = std::thread::spawn(move || -> Result<(Connection, Result<()>)> {
            let mut connection = Connection::open(first_path)?;
            AFTER_READ.with(|hook| {
                hook.set(Some(Box::new(move || {
                    let _ = ready.send(());
                    let _ = wait_first.recv_timeout(Duration::from_secs(10));
                })))
            });
            let result = install(&mut connection);
            Ok((connection, result))
        });
        arrived.recv_timeout(Duration::from_secs(10))?;
        let (events, progress) = mpsc::channel();
        let (release_second, wait_second) = mpsc::channel();
        let second_path = path.clone();
        let second = std::thread::spawn(move || -> Result<(Connection, Result<()>)> {
            let mut connection = Connection::open(second_path)?;
            BUSY.with(|context| *context.borrow_mut() = Some((events.clone(), wait_second)));
            connection.busy_handler(Some(busy))?;
            let result = install(&mut connection);
            let _ = events.send(Progress::Finished);
            Ok((connection, result))
        });
        let observed = progress.recv_timeout(Duration::from_secs(10));
        let _ = release_first.send(());
        let first = first.join();
        let _ = release_second.send(());
        let second = second.join();
        assert!(matches!(observed, Ok(Progress::Contended)), "{observed:?}");
        let (first, first_result) = first.unwrap()?;
        let (second, second_result) = second.unwrap()?;
        first_result?;
        second_result?;
        let empty = read(&first, "selected")?;
        assert_eq!(read(&second, "selected")?, empty);
        first.execute(
            "INSERT INTO acp_events(session_id, seq, event_json, created_at) VALUES ('selected', 1, '{}', 0)",
            [],
        )?;
        let changed = read(&second, "selected")?;
        assert_ne!(changed, empty);
        drop((first, second));
        let mut reopened = Connection::open(path)?;
        install(&mut reopened)?;
        assert_eq!(read(&reopened, "selected")?, changed);
        Ok(())
    }

    #[test]
    fn identity_tracks_content_across_connections_and_survives_empty_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        let first = EventStore::open(&path, 0).unwrap();
        let second = EventStore::open(&path, 0).unwrap();
        let identity = |session| read(&first.conn(), session).unwrap();
        let empty = identity("selected");
        let other = identity("other");
        let event = Event::AgentMessageChunk {
            text: "original".into(),
            message_id: None,
        };
        first.record("selected", 1, &event).unwrap();
        let original = identity("selected");
        assert_ne!(original, empty);
        assert_eq!(read(&second.conn(), "selected").unwrap(), original);
        first
            .record("selected", 1, &Event::ThinkingStarted)
            .unwrap();
        assert_eq!(identity("selected"), original);
        second
            .conn()
            .execute("UPDATE acp_events SET event_json = event_json", [])
            .unwrap();
        assert_eq!(identity("selected"), original);
        {
            let mut conn = second.conn();
            let tx = conn.transaction().unwrap();
            tx.execute("DELETE FROM acp_events WHERE session_id = 'selected'", [])
                .unwrap();
            tx.rollback().unwrap();
        }
        assert_eq!(identity("selected"), original);
        let (_, snapshot) = first
            .retained_page("selected", 0, None, 1, None)
            .unwrap()
            .unwrap();
        let replacement = serde_json::to_string(&Event::ThinkingStarted).unwrap();
        second
            .conn()
            .execute(
                "UPDATE acp_events SET event_json = ?1 WHERE session_id = 'selected'",
                [&replacement],
            )
            .unwrap();
        let replaced = identity("selected");
        assert_ne!(replaced, original);
        assert!(first
            .retained_page("selected", 1, None, 1, Some(&snapshot))
            .unwrap()
            .is_none());
        assert_eq!(identity("other"), other);
        second
            .conn()
            .execute(
                "UPDATE acp_events SET session_id = 'other' WHERE session_id = 'selected'",
                [],
            )
            .unwrap();
        let moved = identity("selected");
        assert_ne!(moved, replaced);
        assert_ne!(identity("other"), other);
        second.record("selected", 1, &event).unwrap();
        let recreated = identity("selected");
        assert_ne!(recreated, moved);
        first.delete_session_checked("selected").unwrap();
        let deleted = identity("selected");
        assert_ne!(deleted, recreated);
        assert_ne!(deleted, empty);
        assert!(first
            .retained_page("selected", 0, None, 1, Some(&deleted))
            .unwrap()
            .is_some());
        first.record("other", 2, &event).unwrap();
        assert_eq!(identity("selected"), deleted);
        drop(second);
        drop(first);
        let reopened = EventStore::open(&path, 0).unwrap();
        assert_eq!(read(&reopened.conn(), "selected").unwrap(), deleted);
        let fresh = EventStore::open(&dir.path().join("fresh.db"), 0).unwrap();
        assert_ne!(read(&fresh.conn(), "selected").unwrap(), deleted);
    }
}
