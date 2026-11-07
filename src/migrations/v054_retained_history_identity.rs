//! Add durable content identity to retained ACP history.

use anyhow::{Context, Result};
use std::path::Path;
use tracing::info;

pub fn run() -> Result<()> {
    migrate_database(&crate::session::get_app_dir()?.join("acp_events.db"))
}

fn migrate_database(path: &Path) -> Result<()> {
    if !path.try_exists()? {
        return Ok(());
    }
    let mut conn = rusqlite::Connection::open(path)
        .with_context(|| format!("open retained history {}", path.display()))?;
    crate::acp::event_store::identity::install(&mut conn)?;
    info!(target: "migrations.v054", "Added retained history content identity");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::{event_store::EventStore, state::Event};

    #[test]
    fn upgrades_legacy_history_without_changing_content_or_identity_on_reentry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acp_events.db");
        migrate_database(&path).unwrap();
        assert!(!path.exists());
        let schema = crate::events::Schema::new("acp").unwrap();
        let conn = crate::events::open(&path, &schema).unwrap();
        crate::events::insert_event(
            &conn,
            &schema,
            "retained",
            7,
            &serde_json::to_string(&Event::ThinkingStarted).unwrap(),
            1,
        )
        .unwrap();
        drop(conn);
        migrate_database(&path).unwrap();
        let identity = || {
            rusqlite::Connection::open(&path)
                .unwrap()
                .query_row(
                    "SELECT version FROM acp_history_versions WHERE session_id = 'retained'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap()
        };
        let initial = identity();
        migrate_database(&path).unwrap();
        assert_eq!(identity(), initial);
        let store = EventStore::open(&path, 0).unwrap();
        assert!(matches!(
            &store.replay_from("retained", 0)[..],
            [(7, Event::ThinkingStarted)]
        ));
    }
}
