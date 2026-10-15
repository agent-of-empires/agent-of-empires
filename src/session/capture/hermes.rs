//! Hermes session capture from its SQLite `state.db`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use super::{canonicalize_or_raw, MAX_SESSION_ID_LEN};

const HERMES_MAX_ROWS: usize = 4 * 1024;
const HERMES_MAX_SCHEMA_COLUMNS: usize = 1024;
const HERMES_SIGNAL_MAX_BYTES: usize = 16 * 1024;

/// An active CLI session; empty or missing signal columns are `None`.
struct HermesSessionRow {
    id: String,
    cwd: Option<String>,
    git_repo_root: Option<String>,
}

/// Active rows, newest first. `signal_columns_present` is false on a legacy
/// schema with neither `cwd` nor `git_repo_root`.
struct HermesSessionScan {
    rows: Vec<HermesSessionRow>,
    signal_columns_present: bool,
}

/// Picks the conversation to resume. With signal columns, only an exact
/// canonical `cwd` match, then (weaker, may be a sibling subdirectory session)
/// `git_repo_root` match, is eligible; a wrong-project row is never guessed.
/// On a legacy schema only a sole unclaimed row is returned. Unlike `hermes -c`
/// there is no global most-recent fallback.
fn select_hermes_session_id(
    scan: &HermesSessionScan,
    project_path: &str,
    exclusion: &HashSet<String>,
) -> Result<String> {
    if scan.signal_columns_present {
        let needle = canonicalize_or_raw(project_path);
        let matched =
            |signal: Option<&str>| signal.is_some_and(|s| canonicalize_or_raw(s) == needle);
        let signals: [fn(&HermesSessionRow) -> Option<&str>; 2] =
            [|row| row.cwd.as_deref(), |row| row.git_repo_root.as_deref()];
        for signal in signals {
            if let Some(row) = scan
                .rows
                .iter()
                .find(|row| !exclusion.contains(&row.id) && matched(signal(row)))
            {
                return Ok(row.id.clone());
            }
        }
        anyhow::bail!("No active Hermes session found matching project path")
    }
    let mut unclaimed = scan
        .rows
        .iter()
        .map(|row| row.id.as_str())
        .filter(|id| !exclusion.contains(*id));
    match (unclaimed.next(), unclaimed.next()) {
        (None, _) => anyhow::bail!("No active Hermes session found"),
        (Some(id), None) => Ok(id.to_string()),
        _ => anyhow::bail!(
            "Multiple active Hermes sessions without a project signal; starting fresh"
        ),
    }
}

/// Reads active CLI rows read-only. An `Err` (missing, locked, schema mismatch) is retried next tick.
fn read_hermes_sessions_from_sqlite(
    db_path: &Path,
    started_after: Option<f64>,
) -> Result<HermesSessionScan> {
    use rusqlite::{Connection, OpenFlags};

    // SQLITE_OPEN_NOFOLLOW rejects symlinks anywhere in the path (macOS temp
    // roots have one), so canonicalize the parent and guard only the leaf.
    let resolved = match (db_path.parent(), db_path.file_name()) {
        (Some(parent), Some(leaf)) => std::fs::canonicalize(parent)
            .map(|parent| parent.join(leaf))
            .unwrap_or_else(|_| db_path.to_path_buf()),
        _ => db_path.to_path_buf(),
    };
    let conn = Connection::open_with_flags(
        &resolved,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .with_context(|| format!("Failed to open Hermes state.db at {}", db_path.display()))?;
    conn.busy_timeout(Duration::from_millis(100))
        .context("Failed to set Hermes DB busy timeout")?;

    let (has_cwd, has_git_repo_root) = probe_signal_columns(&conn)?;
    let cwd_expr = if has_cwd { "cwd" } else { "NULL" };
    let root_expr = if has_git_repo_root {
        "git_repo_root"
    } else {
        "NULL"
    };
    let sql = format!(
        "SELECT id, {cwd_expr}, {root_expr} FROM sessions \
         WHERE source='cli' AND ended_at IS NULL \
         AND (?1 IS NULL OR started_at > ?1) \
         AND length(id) <= ?2 \
         AND ({cwd_expr} IS NULL OR length({cwd_expr}) <= ?3) \
         AND ({root_expr} IS NULL OR length({root_expr}) <= ?3) \
         ORDER BY started_at DESC, id DESC LIMIT ?4"
    );
    let mut stmt = conn
        .prepare(&sql)
        .context("Hermes sessions table missing or schema mismatch")?;
    let nonempty = |value: Option<String>| value.filter(|value| !value.is_empty());
    let rows = stmt
        .query_map(
            rusqlite::params![
                started_after,
                MAX_SESSION_ID_LEN as i64,
                HERMES_SIGNAL_MAX_BYTES as i64,
                (HERMES_MAX_ROWS + 1) as i64
            ],
            |row| {
                Ok(HermesSessionRow {
                    id: row.get(0)?,
                    cwd: nonempty(row.get(1)?),
                    git_repo_root: nonempty(row.get(2)?),
                })
            },
        )
        .context("Failed to query Hermes sessions table")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("Failed to read Hermes session row")?;
    if rows.len() > HERMES_MAX_ROWS {
        anyhow::bail!("Hermes active session count exceeds the row limit");
    }
    Ok(HermesSessionScan {
        rows,
        signal_columns_present: has_cwd || has_git_repo_root,
    })
}

/// Which signal columns exist; older schemas lack them. A missing table surfaces at the SELECT.
fn probe_signal_columns(conn: &rusqlite::Connection) -> Result<(bool, bool)> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(sessions)")
        .context("Failed to prepare Hermes sessions table probe")?;
    let cols = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .context("Failed to read Hermes sessions table columns")?;
    let (mut has_cwd, mut has_git_repo_root) = (false, false);
    for (index, col) in cols.enumerate() {
        if index >= HERMES_MAX_SCHEMA_COLUMNS {
            anyhow::bail!("Hermes sessions table exceeds the column limit");
        }
        let col = col.context("Failed to read Hermes session column name")?;
        has_cwd |= col == "cwd";
        has_git_repo_root |= col == "git_repo_root";
    }
    Ok((has_cwd, has_git_repo_root))
}

/// Polls the mounted Hermes store for a post-launch, project-scoped session.
pub(crate) fn hermes_poll_fn_sandboxed_store(
    store: PathBuf,
    container_cwd: String,
    instance_id: String,
    capture_floor: SystemTime,
    extra_excludes: HashSet<crate::session::ConversationBinding>,
    source: Option<crate::session::ExecutionBinding>,
) -> impl Fn() -> Option<String> + Send + 'static {
    let started_after = capture_floor
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(f64::MAX);
    move || {
        let root = crate::session::AnchoredDir::open(&store).ok()?;
        let db_relative = Path::new("state.db");
        if !root.regular_exists(db_relative) {
            return None;
        }
        let scan =
            read_hermes_sessions_from_sqlite(&root.path().join(db_relative), Some(started_after))
                .ok()?;
        let exclusion = super::compose_exclusion(&instance_id, &extra_excludes, source.as_ref());
        select_hermes_session_id(&scan, &container_cwd, &exclusion)
            .ok()
            .and_then(super::validated_session_id)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::capture_floor;
    use super::*;

    const SCHEMA: &str = "CREATE TABLE sessions (id TEXT, source TEXT, started_at REAL, ended_at REAL, cwd TEXT, git_repo_root TEXT);";

    fn poll(store: &Path, floor: u64) -> impl Fn() -> Option<String> {
        hermes_poll_fn_sandboxed_store(
            store.to_path_buf(),
            "/workspace".to_string(),
            "current".to_string(),
            capture_floor(floor),
            HashSet::new(),
            None,
        )
    }

    #[test]
    fn poller_claims_only_post_launch_matching_row() {
        let tmp = tempfile::tempdir().unwrap();
        let conn = rusqlite::Connection::open(tmp.path().join("state.db")).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        for (id, started_at, cwd) in [
            ("hermes_stale", 1_000.0, "/workspace"),
            ("hermes_wrong", 4_000.0, "/other"),
            ("hermes_fresh", 3_000.0, "/workspace"),
        ] {
            conn.execute(
                "INSERT INTO sessions VALUES (?1, 'cli', ?2, NULL, ?3, NULL)",
                rusqlite::params![id, started_at, cwd],
            )
            .unwrap();
        }
        drop(conn);
        assert_eq!(poll(tmp.path(), 2_000)().as_deref(), Some("hermes_fresh"));
    }

    #[cfg(unix)]
    #[test]
    fn poller_refuses_symlink_fifo_and_excess_rows() {
        use super::super::test_support::{make_fifo, open_fifo_guard};
        use std::os::unix::fs::symlink;
        use std::time::Instant;

        let store = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_db = outside.path().join("state.db");
        let conn = rusqlite::Connection::open(&outside_db).unwrap();
        conn.execute_batch(&format!(
            "{SCHEMA} INSERT INTO sessions VALUES ('linked', 'cli', 10, NULL, '/workspace', NULL);"
        ))
        .unwrap();
        drop(conn);
        let db = store.path().join("state.db");
        symlink(&outside_db, &db).unwrap();
        let poll = poll(store.path(), 0);
        assert_eq!(poll(), None);
        std::fs::remove_file(&db).unwrap();
        make_fifo(&db);
        let fifo_guard = open_fifo_guard(&db);
        let started = Instant::now();
        assert_eq!(poll(), None);
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(fifo_guard);
        std::fs::remove_file(&db).unwrap();

        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        let transaction = conn.unchecked_transaction().unwrap();
        for index in 0..=HERMES_MAX_ROWS {
            transaction
                .execute(
                    "INSERT INTO sessions VALUES (?1, 'cli', ?2, NULL, '/workspace', NULL)",
                    rusqlite::params![format!("hermes_{index}"), index as f64 + 1.0],
                )
                .unwrap();
        }
        transaction.commit().unwrap();
        drop(conn);
        assert_eq!(poll(), None);
    }
}
