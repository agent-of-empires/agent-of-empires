//! CLI command implementations

pub mod acp;
pub mod acp_history;
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
use anyhow::Result;

/// A client for the Daemon, auto-started when none runs (ADR 0004).
pub(crate) async fn daemon_client() -> Result<crate::acp::client::HttpClient> {
    let endpoint = crate::acp::client::require_daemon().await?;
    Ok(crate::acp::client::HttpClient::new(endpoint)?)
}

/// A profile or project write on the epoch-checked Daemon routes. The Daemon,
/// auto-started when none runs, validates profiles, paths and names against its
/// own store and answers with its own message.
pub(crate) async fn runtime_write(
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value> {
    let endpoint = crate::acp::client::daemon_manager::require_config_daemon().await?;
    let client = crate::acp::client::HttpClient::new(endpoint)?;
    // Its client carries the passphrase session, which a bare endpoint's does not.
    let info = client.daemon_client().await?.runtime_info().await?;
    anyhow::ensure!(
        info.protocol_version == crate::daemon::RUNTIME_PROTOCOL_VERSION,
        "Daemon runtime protocol is incompatible"
    );
    Ok(client
        .mutate_with(method, path, |request| {
            let request = request.header(crate::daemon::RUNTIME_EPOCH_HEADER, &info.epoch);
            match &body {
                Some(body) => request.json(body),
                None => request,
            }
        })
        .await?)
}

/// The fields `resolve_session` matches an identifier against.
pub trait SessionRef {
    fn id(&self) -> &str;
    fn title(&self) -> &str;
    fn project_path(&self) -> &str;
}

impl<T: SessionRef + ?Sized> SessionRef for &T {
    fn id(&self) -> &str {
        T::id(self)
    }
    fn title(&self) -> &str {
        T::title(self)
    }
    fn project_path(&self) -> &str {
        T::project_path(self)
    }
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
    lookup_session(identifier, instances.iter().enumerate())
        .map(|(_, row)| row)
        .map_err(|error| error.for_cli(identifier))
}

pub(crate) fn terminal_text(value: &str) -> std::borrow::Cow<'_, str> {
    let Some((start, _)) = value.char_indices().find(|(_, ch)| ch.is_control()) else {
        return std::borrow::Cow::Borrowed(value);
    };
    let mut clean = String::with_capacity(value.len());
    clean.push_str(&value[..start]);
    clean.extend(value[start..].chars().filter(|ch| !ch.is_control()));
    std::borrow::Cow::Owned(clean)
}

impl crate::daemon::SessionLookupError {
    pub(crate) fn for_cli(self, identifier: &str) -> anyhow::Error {
        match self {
            Self::NotFound => anyhow::anyhow!("Session not found: {}", terminal_text(identifier)),
            Self::ProfileNotFound(profile) => {
                anyhow::anyhow!("Profile '{}' does not exist", terminal_text(&profile))
            }
            Self::Ambiguous { count, candidates } => {
                let mut lines: Vec<_> = candidates
                    .iter()
                    .map(|(id, title)| {
                        format!("  {} ({})", terminal_text(id), terminal_text(title))
                    })
                    .collect();
                lines.sort_unstable();
                if count > lines.len() {
                    lines.push(format!("  ({} more)", count - lines.len()));
                }
                anyhow::anyhow!(
                    "Ambiguous session identifier {:?} matches {} sessions:\n{}\nUse a longer prefix or the full ID.",
                    identifier,
                    count,
                    lines.join("\n")
                )
            }
        }
    }
}

pub(crate) fn lookup_session<'a, S: SessionRef + 'a>(
    identifier: &str,
    mut rows: impl Iterator<Item = (usize, &'a S)> + Clone,
) -> std::result::Result<(usize, &'a S), crate::daemon::SessionLookupError> {
    use crate::daemon::SessionLookupError;
    if let Some(inst) = rows.clone().find(|(_, row)| row.id() == identifier) {
        return Ok(inst);
    }

    let mut prefixes = rows
        .clone()
        .filter(|(_, row)| row.id().starts_with(identifier));
    if let Some(first) = prefixes.next() {
        let Some(second) = prefixes.next() else {
            return Ok(first);
        };
        let candidates: Vec<_> = [first, second]
            .into_iter()
            .chain(prefixes)
            .map(|(_, row)| (row.id().into(), row.title().into()))
            .collect();
        return Err(SessionLookupError::Ambiguous {
            count: candidates.len(),
            candidates,
        });
    }

    if let Some(inst) = rows.clone().find(|(_, row)| row.title() == identifier) {
        return Ok(inst);
    }

    if let Some(inst) = rows.find(|(_, row)| row.project_path() == identifier) {
        return Ok(inst);
    }

    Err(SessionLookupError::NotFound)
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
