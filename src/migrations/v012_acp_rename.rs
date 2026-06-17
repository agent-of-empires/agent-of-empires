//! Migration v012: retire the "cockpit" terminology for the structured view / acp.
//!
//! The cockpit concept was retired: the ACP-based structured rendering is now
//! the web dashboard's default "structured view", and the tmux rendering is the
//! opt-in "terminal view". The persisted shapes changed accordingly:
//!
//! - Config: the `[cockpit]` section becomes `[acp]`, and the removed master
//!   switch (`enabled`) and `default_for_claude` keys are dropped.
//! - Sessions: the per-instance `cockpit_mode` boolean becomes the `view` enum
//!   (`"structured"` when it was true; dropped when false so the session falls
//!   back to the default `terminal`). The related `cockpit_agent`,
//!   `cockpit_model`, and `cockpit_acp_session_id` fields are renamed to
//!   `agent_name`, `agent_model`, and `acp_session_id`.
//! - Runtime: the detached-worker directory `cockpit-workers/` becomes
//!   `acp-workers/`.
//!
//! Idempotent: re-running on already-migrated data is a no-op.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;
use tracing::{debug, info};

/// Old session-JSON keys that are a straight rename (value preserved).
const SESSION_KEY_RENAMES: &[(&str, &str)] = &[
    ("cockpit_agent", "agent_name"),
    ("cockpit_model", "agent_model"),
    ("cockpit_acp_session_id", "acp_session_id"),
];

/// Config keys dropped from the renamed `[acp]` section: the master switch
/// and the mobile default-for-claude knob no longer exist.
const DROPPED_CONFIG_KEYS: &[&str] = &["enabled", "default_for_claude"];

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    run_in(&app_dir)
}

pub(crate) fn run_in(app_dir: &Path) -> Result<()> {
    // Global config + every profile config.
    migrate_config_file(&app_dir.join("config.toml"))?;
    let profiles_dir = app_dir.join("profiles");
    if profiles_dir.exists() {
        for entry in fs::read_dir(&profiles_dir)? {
            let entry = entry?;
            if entry.path().is_dir() {
                migrate_config_file(&entry.path().join("config.toml"))?;
                migrate_sessions_file(&entry.path().join("sessions.json"))?;
            }
        }
    }
    // Legacy top-level sessions.json (pre-profiles layout).
    migrate_sessions_file(&app_dir.join("sessions.json"))?;

    // Detached-worker runtime directory.
    relocate_workers_dir(app_dir)?;

    // Persisted ACP transcript database (file + tables). Without this, an
    // upgraded binary opens a fresh empty `acp_events.db` and every prior
    // session's history is orphaned in the old `cockpit_events.db`.
    relocate_events_db(app_dir)?;

    Ok(())
}

/// Move the persisted-transcript SQLite database from the old cockpit names to
/// the acp ones. Without this, all transcript history is lost on upgrade even
/// though the session's `view` is preserved.
///
/// Two cases:
/// - **Clean upgrade** (`acp_events.db` absent): rename the file (with its
///   WAL/SHM sidecars) to `acp_events.db` and `ALTER TABLE` the
///   `cockpit_events` / `cockpit_attachments` tables to the acp names. Fast, no
///   row copy.
/// - **Round-trip recovery** (`acp_events.db` already present): a prior
///   PR-branch run opened a fresh empty `acp_events.db` while the real history
///   stayed orphaned in `cockpit_events.db`. Merge the old rows into the new db
///   (`INSERT OR IGNORE`, so it is safe to repeat), then delete the old file.
fn relocate_events_db(app_dir: &Path) -> Result<()> {
    let old = app_dir.join("cockpit_events.db");
    let new = app_dir.join("acp_events.db");
    if !old.exists() {
        // Nothing old to migrate (fresh acp-only install, or already relocated).
        return Ok(());
    }

    if !new.exists() {
        // Clean upgrade: rename file + sidecars together so SQLite stays
        // consistent (a renamed db without its matching `-wal` would lose
        // un-checkpointed writes), then rename the tables in place.
        for suffix in ["", "-wal", "-shm"] {
            let from = app_dir.join(format!("cockpit_events.db{suffix}"));
            if from.exists() {
                fs::rename(&from, app_dir.join(format!("acp_events.db{suffix}")))?;
            }
        }
        let conn = rusqlite::Connection::open(&new)
            .with_context(|| format!("open events db at {} during v012", new.display()))?;
        for (old_tbl, new_tbl) in [
            ("cockpit_events", "acp_events"),
            ("cockpit_attachments", "acp_attachments"),
        ] {
            if table_exists(&conn, "main", old_tbl)? && !table_exists(&conn, "main", new_tbl)? {
                conn.execute_batch(&format!("ALTER TABLE {old_tbl} RENAME TO {new_tbl};"))?;
            }
        }
        conn.execute_batch(
            "DROP INDEX IF EXISTS idx_cockpit_events_session_seq;
             DROP INDEX IF EXISTS idx_cockpit_events_session_created_at;
             DROP INDEX IF EXISTS idx_cockpit_attachments_session_seq;",
        )?;
        info!(
            "v012: relocated {} -> {} and renamed cockpit_* tables",
            old.display(),
            new.display()
        );
        return Ok(());
    }

    // Round-trip recovery: both files exist. Merge the old transcript into the
    // (possibly empty) new db, keyed by the unchanged session id, then drop the
    // orphaned old file.
    let conn = rusqlite::Connection::open(&new)
        .with_context(|| format!("open events db at {} during v012", new.display()))?;
    // The upgraded binary creates these on open, but the migration may run
    // before that (or against an old db), so ensure they exist first.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS acp_events (
            session_id TEXT NOT NULL, seq INTEGER NOT NULL,
            event_json TEXT NOT NULL, created_at INTEGER NOT NULL,
            PRIMARY KEY (session_id, seq));
         CREATE TABLE IF NOT EXISTS acp_attachments (
            session_id TEXT NOT NULL, seq INTEGER NOT NULL, attachment_id TEXT NOT NULL,
            kind TEXT NOT NULL, mime_type TEXT NOT NULL, name TEXT,
            data BLOB NOT NULL, created_at INTEGER NOT NULL,
            PRIMARY KEY (session_id, attachment_id));",
    )?;
    conn.execute(
        "ATTACH DATABASE ?1 AS src",
        [old.to_string_lossy().as_ref()],
    )?;
    // The orphaned db's tables may carry either the cockpit or acp name
    // depending on how the prior run left it; copy from whichever exists.
    let src_events = if table_exists(&conn, "src", "cockpit_events")? {
        Some("cockpit_events")
    } else if table_exists(&conn, "src", "acp_events")? {
        Some("acp_events")
    } else {
        None
    };
    if let Some(t) = src_events {
        conn.execute_batch(&format!(
            "INSERT OR IGNORE INTO acp_events (session_id, seq, event_json, created_at)
             SELECT session_id, seq, event_json, created_at FROM src.{t};"
        ))?;
    }
    let src_att = if table_exists(&conn, "src", "cockpit_attachments")? {
        Some("cockpit_attachments")
    } else if table_exists(&conn, "src", "acp_attachments")? {
        Some("acp_attachments")
    } else {
        None
    };
    if let Some(t) = src_att {
        conn.execute_batch(&format!(
            "INSERT OR IGNORE INTO acp_attachments
             (session_id, seq, attachment_id, kind, mime_type, name, data, created_at)
             SELECT session_id, seq, attachment_id, kind, mime_type, name, data, created_at
             FROM src.{t};"
        ))?;
    }
    conn.execute("DETACH DATABASE src", [])?;
    drop(conn);
    for suffix in ["", "-wal", "-shm"] {
        let f = app_dir.join(format!("cockpit_events.db{suffix}"));
        if f.exists() {
            fs::remove_file(&f)?;
        }
    }
    info!(
        "v012: merged orphaned {} into {} and removed the old file",
        old.display(),
        new.display()
    );
    Ok(())
}

/// Whether `<schema>.<table>` exists (use `"main"` for the primary connection,
/// or an attached schema name).
fn table_exists(conn: &rusqlite::Connection, schema: &str, table: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM {schema}.sqlite_master WHERE type = 'table' AND name = ?1"),
        [table],
        |row| row.get(0),
    )?;
    Ok(n > 0)
}

/// Rename the `[cockpit]` table to `[acp]` and drop the removed keys.
fn migrate_config_file(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let content = fs::read_to_string(path)?;
    let mut doc: toml::Table = match content.parse() {
        Ok(table) => table,
        Err(e) => {
            debug!("failed to parse {}: {e}, skipping", path.display());
            return Ok(());
        }
    };

    let Some(mut cockpit) = doc.remove("cockpit") else {
        return Ok(());
    };
    if let Some(table) = cockpit.as_table_mut() {
        for key in DROPPED_CONFIG_KEYS {
            table.remove(*key);
        }
    }
    // If a migrated `[acp]` somehow already exists, prefer it and discard the
    // stale `[cockpit]` rather than clobbering.
    doc.entry("acp".to_string()).or_insert(cockpit);

    crate::session::atomic_write(path, toml::to_string_pretty(&doc)?.as_bytes())?;
    info!("v012: renamed [cockpit] -> [acp] in {}", path.display());
    Ok(())
}

/// Rename/transform the per-instance cockpit_* keys in a sessions.json array.
fn migrate_sessions_file(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let content = fs::read_to_string(path)?;
    let mut value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            debug!("failed to parse {}: {e}, skipping", path.display());
            return Ok(());
        }
    };

    let mut changed = false;
    if let Some(array) = value.as_array_mut() {
        for instance in array.iter_mut() {
            if let Some(obj) = instance.as_object_mut() {
                // `cockpit_mode` (bool) -> `view` enum. `view` skips
                // serializing the default `Terminal`, so we only write `view`
                // when the old flag was true (structured); a false/absent flag
                // leaves the session on the default terminal view.
                if let Some(old) = obj.remove("cockpit_mode") {
                    if old.as_bool() == Some(true) {
                        obj.entry("view".to_string())
                            .or_insert(serde_json::Value::String("structured".to_string()));
                    }
                    changed = true;
                }
                for (old, new) in SESSION_KEY_RENAMES {
                    if let Some(v) = obj.remove(*old) {
                        obj.entry((*new).to_string()).or_insert(v);
                        changed = true;
                    }
                }
            }
        }
    }

    if changed {
        crate::session::atomic_write(path, serde_json::to_string_pretty(&value)?.as_bytes())?;
        info!(
            "v012: migrated cockpit_* session keys in {}",
            path.display()
        );
    }
    Ok(())
}

/// Move `cockpit-workers/` to `acp-workers/`. These are ephemeral runtime
/// records (PID/socket/log per detached worker); moving them keeps any
/// in-flight worker reachable across the upgrade.
fn relocate_workers_dir(app_dir: &Path) -> Result<()> {
    let old = app_dir.join("cockpit-workers");
    let new = app_dir.join("acp-workers");
    if !old.exists() {
        return Ok(());
    }
    if new.exists() {
        // New dir already present (fresh start after upgrade): drop the stale
        // old one rather than merging ambiguous worker records.
        fs::remove_dir_all(&old)?;
        debug!("v012: removed stale cockpit-workers/ ({})", old.display());
        return Ok(());
    }
    fs::rename(&old, &new)?;
    info!("v012: relocated {} -> {}", old.display(), new.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renames_config_section_and_drops_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[other]\nk = \"v\"\n\n[cockpit]\nenabled = true\ndefault_for_claude = false\ndefault_agent = \"aoe-agent\"\nmax_concurrent_workers = 5\n",
        )
        .unwrap();

        migrate_config_file(&path).unwrap();

        let doc: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert!(!doc.contains_key("cockpit"));
        let acp = doc["acp"].as_table().unwrap();
        assert!(!acp.contains_key("enabled"));
        assert!(!acp.contains_key("default_for_claude"));
        assert_eq!(acp["default_agent"].as_str(), Some("aoe-agent"));
        assert_eq!(acp["max_concurrent_workers"].as_integer(), Some(5));
        assert!(doc.contains_key("other"));

        // Idempotent.
        migrate_config_file(&path).unwrap();
        let doc2: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert!(!doc2.contains_key("cockpit"));
        assert!(doc2.contains_key("acp"));
    }

    #[test]
    fn migrates_session_keys_and_view_enum() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        fs::write(
            &path,
            r#"[
              {"id":"a","cockpit_mode":true,"cockpit_agent":"claude-code","cockpit_model":"opus","cockpit_acp_session_id":"x"},
              {"id":"b","cockpit_mode":false,"cockpit_agent":"gemini"},
              {"id":"c","title":"plain tmux"}
            ]"#,
        )
        .unwrap();

        migrate_sessions_file(&path).unwrap();

        let arr: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        // structured session: cockpit_mode=true -> view="structured".
        let a = &arr[0];
        assert_eq!(a["view"], serde_json::json!("structured"));
        assert_eq!(a["agent_name"], serde_json::json!("claude-code"));
        assert_eq!(a["agent_model"], serde_json::json!("opus"));
        assert_eq!(a["acp_session_id"], serde_json::json!("x"));
        assert!(a.get("cockpit_mode").is_none());
        assert!(a.get("cockpit_agent").is_none());
        // terminal session: cockpit_mode=false -> no `view` key (default terminal).
        let b = &arr[1];
        assert!(b.get("cockpit_mode").is_none());
        assert!(b.get("view").is_none());
        assert_eq!(b["agent_name"], serde_json::json!("gemini"));
        // A plain session without the keys is untouched.
        assert_eq!(arr[2]["title"], serde_json::json!("plain tmux"));

        // Idempotent.
        migrate_sessions_file(&path).unwrap();
        let arr2: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(arr2[0]["view"], serde_json::json!("structured"));
        assert!(arr2[1].get("view").is_none());
    }

    #[test]
    fn relocates_workers_dir() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("cockpit-workers");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("s1.json"), "{}").unwrap();

        relocate_workers_dir(dir.path()).unwrap();

        assert!(!old.exists());
        assert!(dir.path().join("acp-workers").join("s1.json").exists());
    }

    #[test]
    fn relocates_events_db_renaming_file_and_tables() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("cockpit_events.db");
        // Seed an old-shape transcript db: cockpit_* tables, indexes, one row.
        {
            let conn = rusqlite::Connection::open(&old).unwrap();
            conn.execute_batch(
                "CREATE TABLE cockpit_events (session_id TEXT, seq INTEGER, event_json TEXT, created_at INTEGER, PRIMARY KEY (session_id, seq));
                 CREATE INDEX idx_cockpit_events_session_seq ON cockpit_events(session_id, seq);
                 CREATE INDEX idx_cockpit_events_session_created_at ON cockpit_events(session_id, created_at);
                 CREATE TABLE cockpit_attachments (session_id TEXT, attachment_id TEXT, PRIMARY KEY (session_id, attachment_id));
                 CREATE INDEX idx_cockpit_attachments_session_seq ON cockpit_attachments(session_id);
                 INSERT INTO cockpit_events VALUES ('sess-1', 1, '{\"x\":1}', 42);",
            )
            .unwrap();
        }

        relocate_events_db(dir.path()).unwrap();

        let new = dir.path().join("acp_events.db");
        assert!(!old.exists(), "old db file should be renamed away");
        assert!(new.exists(), "new db file should exist");
        let conn = rusqlite::Connection::open(&new).unwrap();
        // Table renamed, row preserved (history intact).
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM acp_events WHERE session_id = 'sess-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "transcript row must survive the rename");
        // Old table name is gone; new attachment table exists.
        let old_tbl: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='cockpit_events'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old_tbl, 0);
        let att: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='acp_attachments'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(att, 1);

        // Idempotent: a second run is a no-op (new db already present, old gone).
        relocate_events_db(dir.path()).unwrap();
        assert!(new.exists());
    }

    #[test]
    fn relocates_events_db_merges_orphaned_history_on_round_trip() {
        // Simulates a round-trip: a prior PR-branch run left an empty
        // `acp_events.db` while the real history stayed in `cockpit_events.db`.
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("cockpit_events.db");
        let new = dir.path().join("acp_events.db");
        {
            // Orphaned old db with real history.
            let oc = rusqlite::Connection::open(&old).unwrap();
            oc.execute_batch(
                "CREATE TABLE cockpit_events (session_id TEXT, seq INTEGER, event_json TEXT, created_at INTEGER, PRIMARY KEY (session_id, seq));
                 INSERT INTO cockpit_events VALUES ('s1', 1, '{\"a\":1}', 10), ('s1', 2, '{\"a\":2}', 20);",
            )
            .unwrap();
            // Empty new db created by the prior PR run (acp schema, no rows).
            let nc = rusqlite::Connection::open(&new).unwrap();
            nc.execute_batch(
                "CREATE TABLE acp_events (session_id TEXT, seq INTEGER, event_json TEXT, created_at INTEGER, PRIMARY KEY (session_id, seq));",
            )
            .unwrap();
        }

        relocate_events_db(dir.path()).unwrap();

        assert!(!old.exists(), "orphaned old db must be removed after merge");
        let conn = rusqlite::Connection::open(&new).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM acp_events WHERE session_id = 's1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2, "both orphaned rows must be merged into acp_events");
    }
}
