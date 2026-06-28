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
/// UI transcript stops purged rows from orphaning. No-op for non-structured
/// sessions or when the store does not exist. See #2489.
pub(crate) fn purge_acp_transcript(inst: &Instance) -> Result<()> {
    if !inst.is_structured() {
        return Ok(());
    }
    // The durable transcript only exists under the `serve` feature (the `acp`
    // module is gated on it). A default build cannot reach the event store, so
    // it must NOT report success: callers (`rm --purge`, `empty-trash`) read
    // `Ok(())` as "safe to drop the session row", which would delete the row
    // while orphaning its transcript in `acp_events.db`. Bail instead.
    #[cfg(not(feature = "serve"))]
    {
        anyhow::bail!("acp transcript purge requires a build with the `serve` feature")
    }
    #[cfg(feature = "serve")]
    {
        let app_dir = crate::session::get_app_dir()
            .map_err(|e| anyhow::anyhow!("acp transcript purge: resolve app dir: {e}"))?;
        let db_path = app_dir.join("acp_events.db");
        if !db_path.exists() {
            return Ok(());
        }
        let store = crate::acp::event_store::EventStore::open(&db_path, 100)
            .map_err(|e| anyhow::anyhow!("acp transcript purge: open event store: {e}"))?;
        store.delete_session(&inst.id);
        Ok(())
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
}
