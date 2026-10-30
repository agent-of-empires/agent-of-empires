//! CLI command implementations

pub mod acp;
pub mod add;
pub mod agents;
pub mod cityhall;
pub mod definition;
pub mod extract_session_id;
pub mod graft;
pub mod group;
pub mod hooks;
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
pub mod runtime_read;
pub mod sandbox;
pub mod send;
pub mod serve;
pub mod session;
pub mod session_move;
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

pub use definition::{command_name, Cli, Commands, DaemonCommands, CLI_COMMAND_NAMES};

/// Ends the process with this status; the command already printed its output.
#[derive(Debug, thiserror::Error)]
#[error("exit status {0}")]
pub struct Exit(pub i32);

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

/// The fields `resolve_session` matches an identifier against.
pub trait SessionRef {
    fn id(&self) -> &str;
    fn title(&self) -> &str;
    fn project_path(&self) -> &str;
}

impl SessionRef for Instance {
    fn id(&self) -> &str {
        &self.id
    }
    fn title(&self) -> &str {
        &self.title
    }
    fn project_path(&self) -> &str {
        &self.project_path
    }
}

impl SessionRef for crate::daemon::SessionResponse {
    fn id(&self) -> &str {
        &self.id
    }
    fn title(&self) -> &str {
        &self.title
    }
    fn project_path(&self) -> &str {
        &self.project_path
    }
}

pub fn resolve_session<'a, S: SessionRef>(identifier: &str, instances: &'a [S]) -> Result<&'a S> {
    if let Some(inst) = instances.iter().find(|i| i.id() == identifier) {
        return Ok(inst);
    }

    let prefix_matches: Vec<&S> = instances
        .iter()
        .filter(|i| i.id().starts_with(identifier))
        .collect();
    match prefix_matches.len() {
        0 => {}
        1 => return Ok(prefix_matches[0]),
        _ => {
            let mut candidates: Vec<String> = prefix_matches
                .iter()
                .map(|i| format!("  {} ({})", i.id(), i.title()))
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

    if let Some(inst) = instances.iter().find(|i| i.title() == identifier) {
        return Ok(inst);
    }

    if let Some(inst) = instances.iter().find(|i| i.project_path() == identifier) {
        return Ok(inst);
    }

    bail!("Session not found: {}", identifier)
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
    fn resolve_session_matches_daemon_rows() {
        let row = |id: &str, title: &str, path: &str| -> crate::daemon::SessionResponse {
            serde_json::from_value(serde_json::json!({
                "id": id, "title": title, "project_path": path
            }))
            .unwrap()
        };
        let rows = [
            row("abc1", "one", "/p/one"),
            row("abc2", "two", "/p/two"),
            row("xyz9", "three", "/p/three"),
        ];
        for (identifier, expected) in [
            ("abc2", "abc2"),
            ("xy", "xyz9"),
            ("one", "abc1"),
            ("/p/two", "abc2"),
        ] {
            assert_eq!(resolve_session(identifier, &rows).unwrap().id, expected);
        }
        let ambiguous = resolve_session("abc", &rows).unwrap_err().to_string();
        assert!(ambiguous.contains("matches 2 sessions"), "{ambiguous}");
        assert!(resolve_session("nope", &rows).is_err());
    }

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
}
