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

    // The most recently written `opencode.db` or `opencode-<name>.db`.
    let data_dir = opencode_data_dir()?;
    let newest = std::fs::read_dir(&data_dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_name().to_str().is_some_and(|name| {
                name == "opencode.db" || (name.starts_with("opencode-") && name.ends_with(".db"))
            })
        })
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path);
    Some(newest.unwrap_or_else(|| data_dir.join("opencode.db")))
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
    // OpenCode 2.x moved messages to `session_message`, where the role is a
    // column and the error node sits at `$.error.message` instead of
    // `$.error.data.message`. The table name tracks the build channel rather
    // than the version, so each layout is read with its own statement.
    for sql in [
        "SELECT json_extract(data, '$.error.message')
         FROM session_message
         WHERE session_id = ?1
           AND type = 'assistant'
           AND time_created >= ?2
           AND json_extract(data, '$.error.message') IS NOT NULL
         ORDER BY time_created DESC
         LIMIT 1",
        "SELECT json_extract(data, '$.error.data.message')
         FROM message
         WHERE session_id = ?1
           AND json_extract(data, '$.role') = 'assistant'
           AND CAST(json_extract(data, '$.time.created') AS INTEGER) >= ?2
           AND json_extract(data, '$.error.data.message') IS NOT NULL
         ORDER BY CAST(json_extract(data, '$.time.created') AS INTEGER) DESC
         LIMIT 1",
    ] {
        let mut stmt = match conn.prepare(sql) {
            Ok(stmt) => stmt,
            // The store predates the table, so the next candidate decides.
            Err(rusqlite::Error::SqliteFailure(_, Some(reason)))
                if reason.contains("no such table") =>
            {
                continue
            }
            Err(_) => continue,
        };
        let message: String = stmt
            .query_row(
                rusqlite::params![acp_session_id, prompt_started_at_ms],
                |row| row.get(0),
            )
            .ok()?;
        let trimmed = message.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
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

    /// The newest error for this session at or after the prompt started. Rows
    /// from another session, from before the prompt, or with no error message
    /// are all skipped.
    #[test]
    fn recover_opencode_prompt_error_from_sqlite_picks_the_latest_match() {
        for (rows, want) in [
            (
                vec![
                    ("ses-1", 99, Some("old error")),
                    ("ses-1", 100, None),
                    ("ses-1", 110, Some("new error")),
                    ("ses-2", 120, Some("wrong session")),
                ],
                Some("new error"),
            ),
            (
                vec![
                    ("ses-1", 90, Some("too early")),
                    ("ses-1", 100, None),
                    ("ses-2", 110, Some("wrong session")),
                ],
                None,
            ),
        ] {
            let dir = create_opencode_error_test_db(&rows);
            let db_path = dir.path().join("opencode.db");
            let got = recover_opencode_prompt_error_from_sqlite_at(&db_path, "ses-1", 100);
            assert_eq!(got.as_deref(), want);
        }
    }

    /// OpenCode 2.x stores messages in `session_message`, with the role as a
    /// column and the error message at `$.error.message`.
    #[test]
    fn recover_opencode_prompt_error_reads_the_v2_message_table() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("opencode.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session_message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                type TEXT NOT NULL,
                seq INTEGER NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );",
        )
        .unwrap();
        let assistant = |seq: i64, created: i64, error: Option<&str>| {
            let mut data = serde_json::json!({ "time": { "created": created }, "agent": "build", "content": [] });
            if let Some(message) = error {
                data["error"] =
                    serde_json::json!({ "name": "ProviderAuthError", "message": message });
            }
            (
                format!("msg-{seq}"),
                "ses-1".to_string(),
                "assistant".to_string(),
                seq,
                created,
                created,
                data.to_string(),
            )
        };
        let mut rows = vec![
            assistant(1, 99, Some("old error")),
            assistant(2, 100, None),
            assistant(3, 110, Some("new error")),
        ];
        // A non-assistant row carrying an error is not an assistant error.
        rows.push((
            "msg-4".to_string(),
            "ses-1".to_string(),
            "system".to_string(),
            4,
            111,
            111,
            serde_json::json!({ "error": { "message": "system error" } }).to_string(),
        ));
        rows.push((
            "msg-5".to_string(),
            "ses-2".to_string(),
            "assistant".to_string(),
            5,
            120,
            120,
            serde_json::json!({ "error": { "message": "wrong session" } }).to_string(),
        ));
        for row in rows {
            conn.execute(
                "INSERT INTO session_message VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![row.0, row.1, row.2, row.3, row.4, row.5, row.6],
            )
            .unwrap();
        }
        assert_eq!(
            recover_opencode_prompt_error_from_sqlite_at(&db_path, "ses-1", 100).as_deref(),
            Some("new error")
        );
    }

    /// A store with neither table carries no recoverable detail.
    #[test]
    fn recover_opencode_prompt_error_returns_none_without_a_message_table() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("opencode.db");
        Connection::open(&db_path)
            .unwrap()
            .execute_batch("CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT)")
            .unwrap();
        assert_eq!(
            recover_opencode_prompt_error_from_sqlite_at(&db_path, "ses-1", 0),
            None
        );
    }
}
