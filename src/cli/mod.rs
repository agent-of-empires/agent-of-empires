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
pub mod output;
pub mod plugin;
pub mod profile;
pub mod project;
pub mod ps;
pub mod remove;
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

/// Whether CLI stdout should contain ANSI color. Color is terminal-only and
/// follows the NO_COLOR convention (only a non-empty value disables it).
pub(crate) fn color_enabled() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
}

/// One rendered lifecycle-notice line for CLI listings. Amber on a color-
/// capable terminal; plain text otherwise, so pipes and CI logs stay clean.
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
    // Try exact ID match. Exact matches always win over prefix matches and
    // can never be ambiguous (IDs are unique).
    if let Some(inst) = instances.iter().find(|i| i.id == identifier) {
        return Ok(inst);
    }

    // Try ID prefix match. If more than one session has an ID starting with
    // `identifier`, fail loudly instead of silently mutating the first one.
    // Mutating commands (archive, kill, snooze) could otherwise act on the
    // wrong session when the user provides a too-short prefix.
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

    // Try exact title match
    if let Some(inst) = instances.iter().find(|i| i.title == identifier) {
        return Ok(inst);
    }

    // Try path match
    if let Some(inst) = instances.iter().find(|i| i.project_path == identifier) {
        return Ok(inst);
    }

    bail!("Session not found: {}", identifier)
}

/// Best-effort deletion of a structured-view session's durable transcript
/// (the ACP event-store rows under `<app_dir>/acp_events.db`) during a CLI
/// permanent purge (`aoe rm --purge`, `aoe session empty-trash`). The serve
/// daemon does this through its supervisor; the CLI has no live worker, so it
/// opens the event store directly. It cannot send the adapter `session/delete`
/// RPC the daemon sends (that needs a running worker), but deleting the local
/// UI transcript stops purged rows from orphaning. No-op when the store does
/// not exist; a failure to open or write it returns `Err` so callers keep the
/// session row rather than orphan its transcript. See #2489, #2524.
///
/// The delete is idempotent and deliberately does NOT gate on
/// `Instance::is_structured()`: deleting zero rows for a terminal session is
/// harmless, and the old guard orphaned transcripts (#2524).
pub(crate) fn purge_acp_transcript(inst: &Instance) -> Result<()> {
    let app_dir = crate::session::get_app_dir()
        .map_err(|e| anyhow::anyhow!("acp transcript purge: resolve app dir: {e}"))?;
    let db_path = app_dir.join("acp_events.db");
    if !db_path.exists() {
        return Ok(());
    }
    purge_acp_transcript_rows(&db_path, &inst.id)
}

/// Delete a session's rows from the ACP event store at `db_path`, removing both
/// the event rows and their attachment blobs (mirrors
/// `crate::events::delete_topic`'s cascade so no orphaned bytes are left).
/// A missing table means the store predates it: nothing to purge.
fn purge_acp_transcript_rows(db_path: &std::path::Path, session_id: &str) -> Result<()> {
    let mut conn = rusqlite::Connection::open(db_path)
        .map_err(|e| anyhow::anyhow!("acp transcript purge: open event store: {e}"))?;
    // A running daemon may hold the store open; wait briefly rather than fail.
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("acp transcript purge: set busy_timeout: {e}"))?;
    // Both deletes run in one transaction so the purge is all-or-nothing: if the
    // attachments delete fails after the events delete, the dropped `tx` rolls
    // both back and the caller keeps the session row for retry rather than
    // leaving the transcript half removed.
    let tx = conn
        .transaction()
        .map_err(|e| anyhow::anyhow!("acp transcript purge: begin transaction: {e}"))?;
    // Table names come from the same `crate::events::Schema` the store is
    // opened with, so they cannot drift; `session_id` is bound, so the
    // `format!` only interpolates a validated constant.
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

/// Aggregated `empty-trash` outcome across per-session purge transactions.
/// Named rather than positional because every field is a `usize`.
pub(crate) struct EmptyTrashOutcome {
    /// Successfully-purged rows dropped from storage.
    pub removed: usize,
    /// Rows a peer restored AFTER our teardown began (orphan-risk; the caller
    /// warns). Kept, not dropped.
    pub restored_after_teardown: usize,
    /// Rows WE claimed whose teardown/transcript purge failed and are still
    /// trashed: genuinely kept for retry (distinct from peer restores).
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

/// Resolve `identifier` and run `f` on the matching instance. Designed for
/// use inside `Storage::update`'s closure: find + mutate is atomic under
/// both lock layers. Delegates to `resolve_session`, so ambiguous prefixes
/// error rather than silently picking the first match.
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
    fn truncate_id_shorter_than_max_returns_input() {
        assert_eq!(truncate_id("abc", 8), "abc");
    }

    #[test]
    fn truncate_id_equal_to_max_returns_input() {
        assert_eq!(truncate_id("abcdefgh", 8), "abcdefgh");
    }

    #[test]
    fn truncate_id_ascii_truncates_to_max_chars() {
        assert_eq!(truncate_id("abcdefghij", 8), "abcdefgh");
    }

    #[test]
    fn truncate_id_multibyte_does_not_panic_and_respects_char_boundary() {
        // "café" is 4 chars / 5 bytes. The naive byte-slice version would have
        // panicked on max_len=4 mid-codepoint.
        assert_eq!(truncate_id("café", 3), "caf");
        assert_eq!(truncate_id("café", 4), "café");
        assert_eq!(truncate_id("café", 10), "café");
    }

    #[test]
    fn truncate_id_zero_max_returns_empty() {
        assert_eq!(truncate_id("abc", 0), "");
        assert_eq!(truncate_id("café", 0), "");
    }

    #[test]
    fn patch_instance_exact_id_resolves_unambiguously() {
        let mut v = vec![
            Instance::new("first", "/tmp/a"),
            Instance::new("second", "/tmp/b"),
        ];
        let target_id = v[1].id.clone();
        patch_instance(&mut v, &target_id, |i| {
            i.title = "hit".to_string();
            Ok(())
        })
        .unwrap();
        assert_eq!(v[1].title, "hit");
        assert_eq!(v[0].title, "first");
    }

    #[test]
    fn patch_instance_rejects_ambiguous_prefix() {
        let mut v = vec![
            Instance::new("first", "/tmp/a"),
            Instance::new("second", "/tmp/b"),
        ];
        v[0].id = "abcdef-1".to_string();
        v[1].id = "abcdef-2".to_string();
        let err = patch_instance(&mut v, "abcdef", |_| Ok(())).unwrap_err();
        assert!(
            err.to_string().contains("Ambiguous"),
            "expected ambiguity error, got: {err}"
        );
    }

    #[test]
    fn patch_instance_resolves_by_title() {
        let mut v = vec![
            Instance::new("alpha", "/tmp/a"),
            Instance::new("beta", "/tmp/b"),
        ];
        patch_instance(&mut v, "beta", |i| {
            i.title = "renamed".to_string();
            Ok(())
        })
        .unwrap();
        assert_eq!(v[1].title, "renamed");
    }

    // #2534: a purge keeps a targeted row only when it was trashed at snapshot
    // time but is no longer trashed (restored mid-purge); every other case
    // drops it (still trashed, or a direct live purge with no restore to lose).
    #[test]
    fn purge_keeps_only_rows_restored_after_a_trashed_snapshot() {
        assert!(purge_restored_row_must_be_kept(true, false));
        assert!(!purge_restored_row_must_be_kept(true, true));
        assert!(!purge_restored_row_must_be_kept(false, false));
        assert!(!purge_restored_row_must_be_kept(false, true));
    }

    // #2524: the purge path used to be unreachable, orphaning transcripts.
    // The row delete must drop both the event rows and their attachment blobs
    // for the target session only.
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

    // A store that predates a table (or any expected table missing) is not an
    // error: there is simply nothing to purge.
    #[test]
    fn purge_acp_transcript_rows_tolerates_missing_table() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("acp_events.db");
        // Open creates an empty db with neither acp_events nor acp_attachments.
        rusqlite::Connection::open(&db_path).unwrap();
        purge_acp_transcript_rows(&db_path, "whatever").unwrap();
    }
}
