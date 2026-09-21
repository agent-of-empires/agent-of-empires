//! CLI command implementations

pub mod acp;
pub mod add;
pub mod agents;
pub mod cityhall;
pub mod definition;
pub mod extract_session_id;
pub mod graft;
pub mod group;
pub mod init;
pub mod killall;
pub mod list;
pub mod log_level;
pub mod logs;
pub mod mcp;
pub mod migrate;
pub mod output;
pub mod plugin;
pub mod profile;
pub mod project;
pub mod ps;
pub mod remove;
pub mod sandbox;
pub mod send;
pub mod serve;
pub mod session;
pub mod settings;
pub mod skill;
pub mod sounds;
pub mod status;
pub mod telemetry;
pub mod theme;
pub mod tmux;
pub mod uninstall;
pub mod update;
pub mod url;
pub mod worktree;

pub use definition::{command_name, Cli, Commands, CLI_COMMAND_NAMES};

pub(crate) fn color_enabled() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
}

pub(crate) fn lifecycle_notice_line(indent: &str, notice: &str) -> String {
    if color_enabled() {
        format!("{indent}\x1b[33m⚠ {notice}\x1b[0m")
    } else {
        format!("{indent}⚠ {notice}")
    }
}

use crate::session::Instance;
use anyhow::{bail, Result};

pub fn resolve_session<'a>(identifier: &str, instances: &'a [Instance]) -> Result<&'a Instance> {
    if let Some(inst) = instances.iter().find(|i| i.id == identifier) {
        return Ok(inst);
    }

    let prefix_matches: Vec<&Instance> = instances
        .iter()
        .filter(|i| i.id.starts_with(identifier))
        .collect();
    match prefix_matches.len() {
        0 => {}
        1 => return Ok(prefix_matches[0]),
        _ => {
            let mut candidates: Vec<String> = prefix_matches
                .iter()
                .map(|i| format!("  {} ({})", i.id, i.title))
                .collect();
            candidates.sort();
            bail!(
                "Ambiguous session identifier {:?} matches {} sessions:\n{}\nUse a longer prefix or the full ID.",
                identifier,
                prefix_matches.len(),
                candidates.join("\n")
            );
        }
    }

    if let Some(inst) = instances.iter().find(|i| i.title == identifier) {
        return Ok(inst);
    }

    if let Some(inst) = instances.iter().find(|i| i.project_path == identifier) {
        return Ok(inst);
    }

    bail!("Session not found: {}", identifier)
}

pub(crate) fn purge_acp_transcript(inst: &Instance) -> Result<()> {
    let app_dir = crate::session::get_app_dir()
        .map_err(|e| anyhow::anyhow!("acp transcript purge: resolve app dir: {e}"))?;
    let db_path = app_dir.join("acp_events.db");
    if !db_path.exists() {
        return Ok(());
    }
    purge_acp_transcript_rows(&db_path, &inst.id)
}

fn purge_acp_transcript_rows(db_path: &std::path::Path, session_id: &str) -> Result<()> {
    let mut conn = rusqlite::Connection::open(db_path)
        .map_err(|e| anyhow::anyhow!("acp transcript purge: open event store: {e}"))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("acp transcript purge: set busy_timeout: {e}"))?;
    let tx = conn
        .transaction()
        .map_err(|e| anyhow::anyhow!("acp transcript purge: begin transaction: {e}"))?;
    let schema = crate::events::Schema::new("acp")
        .map_err(|e| anyhow::anyhow!("acp transcript purge: schema: {e}"))?;
    for table in [schema.events_table(), schema.attachments_table()] {
        match tx.execute(
            &format!("DELETE FROM {table} WHERE session_id = ?1"),
            rusqlite::params![session_id],
        ) {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(_, Some(msg))) if msg.contains("no such table") => {}
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "acp transcript purge: delete from {table}: {e}"
                ))
            }
        }
    }
    tx.commit()
        .map_err(|e| anyhow::anyhow!("acp transcript purge: commit: {e}"))?;
    Ok(())
}

pub(crate) struct EmptyTrashOutcome {
    pub removed: usize,
    pub restored_after_teardown: usize,
    pub kept_for_retry: usize,
}

pub fn truncate(s: &str, max: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max {
        s.to_string()
    } else if max <= 3 {
        s.chars().take(max).collect()
    } else {
        let truncated: String = s.chars().take(max - 3).collect();
        format!("{}...", truncated)
    }
}

pub fn truncate_id(id: &str, max_len: usize) -> &str {
    match id.char_indices().nth(max_len) {
        Some((byte_pos, _)) => &id[..byte_pos],
        None => id,
    }
}

pub(crate) fn patch_instance<F, R>(instances: &mut [Instance], identifier: &str, f: F) -> Result<R>
where
    F: FnOnce(&mut Instance) -> Result<R>,
{
    let id = resolve_session(identifier, instances)?.id.clone();
    let inst = instances
        .iter_mut()
        .find(|i| i.id == id)
        .expect("resolve_session returned an id that is no longer in instances");
    f(inst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::claim::purge_restored_row_must_be_kept;

    #[test]
    fn truncate_id_clamps_to_char_boundaries() {
        let cases = [
            ("abc", 8, "abc"),
            ("abcdefgh", 8, "abcdefgh"),
            ("abcdefghij", 8, "abcdefgh"),
            ("café", 3, "caf"),
            ("café", 4, "café"),
            ("café", 10, "café"),
            ("abc", 0, ""),
            ("café", 0, ""),
        ];
        for (input, max, expected) in cases {
            assert_eq!(truncate_id(input, max), expected, "{input:?}/{max}");
        }
    }

    #[test]
    fn patch_instance_resolves_by_id_or_title_and_rejects_an_ambiguous_prefix() {
        let rows = || {
            vec![
                Instance::new("alpha", "/tmp/a"),
                Instance::new("beta", "/tmp/b"),
            ]
        };

        let mut v = rows();
        let target_id = v[1].id.clone();
        patch_instance(&mut v, &target_id, |i| {
            i.title = "hit".to_string();
            Ok(())
        })
        .unwrap();
        assert_eq!(v[1].title, "hit");
        assert_eq!(v[0].title, "alpha", "the other row is untouched");

        let mut v = rows();
        patch_instance(&mut v, "beta", |i| {
            i.title = "renamed".to_string();
            Ok(())
        })
        .unwrap();
        assert_eq!(v[1].title, "renamed");

        let mut v = rows();
        v[0].id = "abcdef-1".to_string();
        v[1].id = "abcdef-2".to_string();
        let err = patch_instance(&mut v, "abcdef", |_| Ok(())).unwrap_err();
        assert!(
            err.to_string().contains("Ambiguous"),
            "expected ambiguity error, got: {err}"
        );
    }

    #[test]
    fn purge_keeps_only_rows_restored_after_a_trashed_snapshot() {
        assert!(purge_restored_row_must_be_kept(true, false));
        assert!(!purge_restored_row_must_be_kept(true, true));
        assert!(!purge_restored_row_must_be_kept(false, false));
        assert!(!purge_restored_row_must_be_kept(false, true));
    }

    #[test]
    fn purge_acp_transcript_rows_deletes_only_target_session() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("acp_events.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE acp_events (session_id TEXT, seq INTEGER, event_json TEXT);
             CREATE TABLE acp_attachments (session_id TEXT, attachment_id TEXT, data BLOB);
             INSERT INTO acp_events VALUES ('keep', 0, '{}'), ('drop', 0, '{}'), ('drop', 1, '{}');
             INSERT INTO acp_attachments VALUES ('keep', 'a0', x'00'), ('drop', 'a1', x'01');",
        )
        .unwrap();
        drop(conn);

        purge_acp_transcript_rows(&db_path, "drop").unwrap();

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM acp_events WHERE session_id = 'drop'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let attachments: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM acp_attachments WHERE session_id = 'drop'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let kept_events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM acp_events WHERE session_id = 'keep'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(events, 0, "target event rows should be deleted");
        assert_eq!(attachments, 0, "target attachment blobs should be deleted");
        assert_eq!(kept_events, 1, "other session must be untouched");
    }

    #[test]
    fn purge_acp_transcript_rows_tolerates_missing_table() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("acp_events.db");
        rusqlite::Connection::open(&db_path).unwrap();
        purge_acp_transcript_rows(&db_path, "whatever").unwrap();
    }
}
