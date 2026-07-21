//! CLI command implementations

#[cfg(feature = "serve")]
pub mod acp;
pub mod add;
pub mod agents;
pub mod definition;
pub mod extract_session_id;
pub mod graft;
pub mod group;
pub mod init;
pub mod killall;
pub mod list;
#[cfg(feature = "serve")]
pub mod log_level;
pub mod logs;
pub mod mcp;
pub mod output;
pub mod plugin;
pub mod profile;
pub mod project;
pub mod remove;
pub mod send;
#[cfg(feature = "serve")]
pub mod serve;
pub mod session;
pub mod settings;
pub mod sounds;
pub mod status;
pub mod telemetry;
pub mod theme;
pub mod tmux;
pub mod uninstall;
pub mod update;
#[cfg(feature = "serve")]
pub mod url;
pub mod worktree;

pub use definition::{command_name, Cli, Commands, CLI_COMMAND_NAMES};

use crate::session::{ClaimOp, Instance};
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
/// The delete is idempotent and feature-independent: `rusqlite` is a
/// non-optional dependency, so a default (non-`serve`) build can reach the
/// store too. It deliberately does NOT gate on `Instance::is_structured()`,
/// which always returns `false` in a non-`serve` build (the `view` field is
/// serve-gated), so the old guard left the non-serve bail unreachable and
/// orphaned transcripts. Deleting zero rows for a terminal session is harmless.
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
/// A missing table means the store predates it: nothing to purge. Kept
/// feature-independent so non-`serve` CLI builds can clean transcripts too.
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
    // The source of truth for these names is `crate::events::Schema` (prefix
    // "acp"), which is serve-gated and so cannot be referenced from here. They
    // are fixed literals, not user input, and `session_id` is bound, so the
    // `format!` only interpolates a constant.
    for table in ["acp_events", "acp_attachments"] {
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

/// Apply a completed `empty-trash` purge to the latest storage snapshot under
/// the lock: drop every successfully-purged row that is still trashed, and keep
/// any that a concurrent restore brought back (its teardown already ran, so the
/// caller should warn). Returns `(removed, restored_kept)`. The `removed` count
/// is what callers must report instead of the candidate count. See #2527, #2534.
pub(crate) fn apply_empty_trash_purge(
    instances: &mut Vec<Instance>,
    purged: &std::collections::HashSet<String>,
) -> (usize, usize) {
    let before = instances.len();
    let mut restored = 0usize;
    instances.retain(|i| {
        if !purged.contains(&i.id) {
            return true;
        }
        if crate::session::claim::purge_restored_row_must_be_kept(true, i.is_trashed()) {
            restored += 1;
            true
        } else {
            false
        }
    });
    (before - instances.len(), restored)
}

/// Result of the Phase-2 `empty-trash` finalize under the flock. Named (not a
/// positional tuple) because all three fields are `usize` and the semantics
/// accreted across #2527/#2534/#2541, so a positional swap would be silent.
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

/// Phase-2 finalize for `empty-trash` under the flock: drop successfully-purged
/// rows still trashed, keep rows a restore brought back, and release the Purge
/// claim on every row we claimed (purged-but-restored, or claimed-and-failed),
/// ownership-guarded so a peer's fresh Restore claim survives. See #2527, #2534,
/// #2541.
pub(crate) fn finalize_empty_trash(
    instances: &mut Vec<Instance>,
    purged: &std::collections::HashSet<String>,
    claimed_failed: &std::collections::HashSet<String>,
) -> EmptyTrashOutcome {
    let (removed, restored_after_teardown) = apply_empty_trash_purge(instances, purged);
    let mut kept_for_retry = 0usize;
    for stored in instances.iter_mut() {
        if !(purged.contains(&stored.id) || claimed_failed.contains(&stored.id)) {
            continue;
        }
        stored.clear_op_claim_if_owned(ClaimOp::Purge);
        if claimed_failed.contains(&stored.id) && stored.is_trashed() {
            kept_for_retry += 1;
        }
    }
    EmptyTrashOutcome {
        removed,
        restored_after_teardown,
        kept_for_retry,
    }
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

    // #2527 + #2534: empty-trash must report the count actually removed (not
    // the candidate count), drop only rows still trashed, and keep rows a
    // concurrent restore brought back.
    #[test]
    fn apply_empty_trash_purge_counts_removed_and_keeps_restored() {
        use std::collections::HashSet;

        let mut still_trashed = Instance::new("gone", "/tmp/a");
        still_trashed.trash();
        let restored = Instance::new("restored", "/tmp/b"); // purged but no longer trashed
        let mut untargeted = Instance::new("other", "/tmp/c");
        untargeted.trash();

        let purged: HashSet<String> = [still_trashed.id.clone(), restored.id.clone()]
            .into_iter()
            .collect();
        let restored_id = restored.id.clone();
        let untargeted_id = untargeted.id.clone();
        let mut instances = vec![still_trashed, restored, untargeted];

        let (removed, kept_restored) = apply_empty_trash_purge(&mut instances, &purged);

        assert_eq!(removed, 1, "only the still-trashed candidate is removed");
        assert_eq!(
            kept_restored, 1,
            "the restored candidate is kept and counted"
        );
        let surviving: Vec<&str> = instances.iter().map(|i| i.id.as_str()).collect();
        assert!(surviving.contains(&restored_id.as_str()));
        assert!(surviving.contains(&untargeted_id.as_str()));
        assert_eq!(instances.len(), 2);
    }

    // #2541: empty-trash Phase 2 releases the Purge claim on every row WE
    // claimed (a row a peer restored mid-purge, or one whose teardown failed) so
    // it is not wedged; it never clears a peer's fresh Restore claim; and
    // `kept_for_retry` counts only our failed teardowns, not peer restores.
    #[test]
    fn empty_trash_clears_claim_on_kept_row() {
        use std::collections::HashSet;

        let now = chrono::Utc::now();
        let ttl = Instance::OP_CLAIM_TTL;

        // Purged and still trashed: removed (its claim goes with the row).
        let mut removed_row = Instance::new("removed", "/tmp/a");
        removed_row.trash();
        removed_row.try_claim(ClaimOp::Purge, ttl, now).unwrap();

        // Purged but a peer restored it mid-purge (stale-override): now untrashed
        // and holding the peer's Restore claim. Kept, counted in `restored`, and
        // the peer's Restore claim must survive the ownership-guarded clear.
        let mut restored_row = Instance::new("restored", "/tmp/b");
        restored_row.try_claim(ClaimOp::Restore, ttl, now).unwrap();

        // We claimed it and its teardown failed (in `claimed_failed`): still
        // trashed, still holding OUR Purge claim. Kept for retry, claim released.
        let mut failed_row = Instance::new("failed", "/tmp/c");
        failed_row.trash();
        failed_row.try_claim(ClaimOp::Purge, ttl, now).unwrap();

        let purged: HashSet<String> = [removed_row.id.clone(), restored_row.id.clone()]
            .into_iter()
            .collect();
        let claimed_failed: HashSet<String> = [failed_row.id.clone()].into_iter().collect();
        let restored_id = restored_row.id.clone();
        let failed_id = failed_row.id.clone();
        let mut instances = vec![removed_row, restored_row, failed_row];

        let outcome = finalize_empty_trash(&mut instances, &purged, &claimed_failed);

        assert_eq!(
            outcome.removed, 1,
            "only the still-trashed purged row is removed"
        );
        assert_eq!(
            outcome.restored_after_teardown, 1,
            "the peer-restored purged row is kept and counted"
        );
        assert_eq!(
            outcome.kept_for_retry, 1,
            "only the failed-teardown row is kept for retry; the peer-restored \
             row is reported via `restored_after_teardown`, not as a retry"
        );

        let restored_kept = instances.iter().find(|i| i.id == restored_id).unwrap();
        assert_eq!(
            restored_kept.op_claim.as_ref().map(|c| c.op),
            Some(ClaimOp::Restore),
            "a peer's fresh Restore claim is never cleared by the purge finalize"
        );
        let failed_kept = instances.iter().find(|i| i.id == failed_id).unwrap();
        assert_eq!(
            failed_kept.op_claim, None,
            "our Purge claim is released on the kept failed-teardown row"
        );
    }

    // #2524: the non-serve purge path used to be unreachable, orphaning
    // transcripts. The feature-independent row delete must drop both the
    // event rows and their attachment blobs for the target session only.
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
