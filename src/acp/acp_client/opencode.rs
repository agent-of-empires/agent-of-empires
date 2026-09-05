//! Recovering an OpenCode prompt error from its local SQLite store, which
//! carries detail the ACP response drops.

use std::path::{Path, PathBuf};
use std::time::Duration;

pub(super) fn opencode_data_dir() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("opencode"));
        }
    }
    let home = std::env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("opencode"),
    )
}

pub(super) fn opencode_db_path() -> Option<PathBuf> {
    if let Ok(raw) = std::env::var("OPENCODE_DB") {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed == ":memory:" {
            return None;
        }
        let path = PathBuf::from(trimmed);
        if path.is_absolute() {
            return Some(path);
        }
        return opencode_data_dir().map(|dir| dir.join(path));
    }

    let data_dir = opencode_data_dir()?;
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let entries = std::fs::read_dir(&data_dir).ok()?;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let is_candidate =
            name == "opencode.db" || (name.starts_with("opencode-") && name.ends_with(".db"));
        if !is_candidate {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if best
            .as_ref()
            .map(|(best_mtime, _)| modified > *best_mtime)
            .unwrap_or(true)
        {
            best = Some((modified, path));
        }
    }
    best.map(|(_, path)| path)
        .or_else(|| Some(data_dir.join("opencode.db")))
}

pub(super) fn recover_opencode_prompt_error_from_sqlite_at(
    db_path: &Path,
    acp_session_id: &str,
    prompt_started_at_ms: i64,
) -> Option<String> {
    use rusqlite::{Connection, OpenFlags};

    if !db_path.exists() {
        return None;
    }
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let _ = conn.busy_timeout(Duration::from_millis(100));
    let mut stmt = conn
        .prepare(
            "SELECT json_extract(data, '$.error.data.message')
             FROM message
             WHERE session_id = ?1
               AND json_extract(data, '$.role') = 'assistant'
               AND CAST(json_extract(data, '$.time.created') AS INTEGER) >= ?2
               AND json_extract(data, '$.error.data.message') IS NOT NULL
             ORDER BY CAST(json_extract(data, '$.time.created') AS INTEGER) DESC
             LIMIT 1",
        )
        .ok()?;
    let message: String = stmt
        .query_row(
            rusqlite::params![acp_session_id, prompt_started_at_ms],
            |row| row.get(0),
        )
        .ok()?;
    let trimmed = message.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

pub(super) fn recover_opencode_prompt_error(
    acp_session_id: &str,
    prompt_started_at_ms: i64,
) -> Option<String> {
    let db_path = opencode_db_path()?;
    recover_opencode_prompt_error_from_sqlite_at(&db_path, acp_session_id, prompt_started_at_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn create_opencode_error_test_db(rows: &[(&str, i64, Option<&str>)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("opencode.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );",
        )
        .unwrap();
        for (idx, (session_id, created, error_message)) in rows.iter().enumerate() {
            let data = if let Some(message) = error_message {
                serde_json::json!({
                    "role": "assistant",
                    "time": { "created": created },
                    "error": { "data": { "message": message } },
                })
            } else {
                serde_json::json!({
                    "role": "assistant",
                    "time": { "created": created },
                })
            };
            conn.execute(
                "INSERT INTO message (id, session_id, time_created, time_updated, data)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    format!("msg-{idx}"),
                    session_id,
                    created,
                    created,
                    data.to_string()
                ],
            )
            .unwrap();
        }
        dir
    }

    #[test]
    fn recover_opencode_prompt_error_from_sqlite_returns_latest_matching_error() {
        let dir = create_opencode_error_test_db(&[
            ("ses-1", 99, Some("old error")),
            ("ses-1", 100, None),
            ("ses-1", 110, Some("new error")),
            ("ses-2", 120, Some("wrong session")),
        ]);
        let db_path = dir.path().join("opencode.db");
        let result = recover_opencode_prompt_error_from_sqlite_at(&db_path, "ses-1", 100);
        assert_eq!(result.as_deref(), Some("new error"));
    }

    #[test]
    fn recover_opencode_prompt_error_from_sqlite_returns_none_without_match() {
        let dir = create_opencode_error_test_db(&[
            ("ses-1", 90, Some("too early")),
            ("ses-1", 100, None),
            ("ses-2", 110, Some("wrong session")),
        ]);
        let db_path = dir.path().join("opencode.db");
        let result = recover_opencode_prompt_error_from_sqlite_at(&db_path, "ses-1", 100);
        assert_eq!(result, None);
    }
}
