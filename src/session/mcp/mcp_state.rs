//! Drift store (`<app_dir>/mcp_state.json`): last-seen native MCP definitions per agent.
//!
//! Holds full unredacted definitions so kept or AoE-winning servers can be
//! rebuilt; `locked_update` keeps it owner-only. AoE never writes native configs.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::mcp_model::NativeRead;
use super::project_mcp::ProjectMcpServer;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct McpState {
    /// agent key -> server name -> last-seen definition.
    #[serde(default)]
    native_snapshots: BTreeMap<String, BTreeMap<String, ProjectMcpServer>>,
}

/// A native definition that diverged from AoE's snapshot (`previous`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConflict {
    pub agent: String,
    pub previous: ProjectMcpServer,
    pub current: ProjectMcpServer,
}

impl McpConflict {
    /// Optimistic-concurrency token over both sides, so a change to either after
    /// the surface captured it makes the resolution stale.
    pub fn fingerprint(&self) -> String {
        super::project_mcp::fingerprint(&[self.previous.clone(), self.current.clone()])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictWinner {
    /// Promote AoE's snapshot definition into global `mcp.json`, which outranks native.
    Aoe,
    /// Re-baseline the snapshot to the native definition.
    Native,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveStatus {
    Applied,
    /// Either side moved since the token was captured; nothing changed.
    Stale,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpReconcile {
    pub conflicts: Vec<McpConflict>,
    /// Last-seen definitions no longer in the native config (kept-on-removal).
    pub removed: Vec<ProjectMcpServer>,
    /// A malformed native entry paused drift detection, so it is not misreported as removed.
    pub paused: bool,
}

/// Reconciles a native read against the snapshot. New servers are adopted
/// silently; conflicting and removed servers keep their snapshot value until the
/// user resolves them. Disabled native servers stay in `read.servers`, so toggling
/// is neither removal nor conflict.
pub fn reconcile_agent(agent: &str, read: &NativeRead) -> Result<McpReconcile> {
    if !read.skipped.is_empty() {
        tracing::warn!(
            target: "acp.mcp",
            agent = %agent,
            skipped = read.skipped.len(),
            "native MCP config has malformed entries; pausing drift detection for this agent"
        );
        return Ok(McpReconcile {
            paused: true,
            ..Default::default()
        });
    }

    let current: BTreeMap<&str, &ProjectMcpServer> =
        read.servers.iter().map(|s| (s.name.as_str(), s)).collect();

    with_locked_state(|state| {
        let snapshot = state.native_snapshots.entry(agent.to_string()).or_default();
        let conflicts = current
            .iter()
            .filter_map(|(name, cur)| {
                let prev = snapshot.get(*name).filter(|prev| prev != cur)?;
                Some(McpConflict {
                    agent: agent.to_string(),
                    previous: prev.clone(),
                    current: (*cur).clone(),
                })
            })
            .collect();
        let removed = snapshot
            .iter()
            .filter(|(name, _)| !current.contains_key(name.as_str()))
            .map(|(_, def)| def.clone())
            .collect();
        for (name, cur) in current {
            snapshot
                .entry(name.to_string())
                .or_insert_with(|| cur.clone());
        }
        McpReconcile {
            conflicts,
            removed,
            paused: false,
        }
    })
}

/// Promotes a kept-on-removal server to global `mcp.json`, then forgets its
/// snapshot entry (in that order, so a failed write leaves it keepable).
/// Returns `false` when no such entry exists.
pub fn keep_removed(agent: &str, name: &str) -> Result<bool> {
    let def = with_locked_state(|state| {
        state
            .native_snapshots
            .get(agent)
            .and_then(|m| m.get(name))
            .cloned()
    })?;
    let Some(def) = def else {
        return Ok(false);
    };
    super::mcp_overrides::upsert_global_server(&def)?;
    forget_native(agent, name)?;
    Ok(true)
}

pub fn forget_native(agent: &str, name: &str) -> Result<()> {
    with_locked_state(|state| {
        if let Some(snapshot) = state.native_snapshots.get_mut(agent) {
            snapshot.remove(name);
            if snapshot.is_empty() {
                state.native_snapshots.remove(agent);
            }
        }
    })
}

/// Resolves a freshly reconciled `conflict` if it still matches the token the
/// surface captured and the on-disk snapshot. The snapshot is always re-baselined
/// to native; `Aoe` additionally promotes the old definition to global, only
/// after the re-baseline committed.
pub fn resolve_conflict(
    conflict: &McpConflict,
    winner: ConflictWinner,
    expected_fingerprint: &str,
) -> Result<ResolveStatus> {
    if conflict.fingerprint() != expected_fingerprint {
        return Ok(ResolveStatus::Stale);
    }
    let name = &conflict.current.name;
    let decision = with_locked_state(|state| {
        let snapshot = state.native_snapshots.get_mut(&conflict.agent)?;
        let snap = snapshot.get(name).filter(|s| **s == conflict.previous)?;
        let promote = (winner == ConflictWinner::Aoe).then(|| snap.clone());
        snapshot.insert(name.clone(), conflict.current.clone());
        Some(promote)
    })?;
    let Some(promote) = decision else {
        return Ok(ResolveStatus::Stale);
    };
    if let Some(def) = promote {
        super::mcp_overrides::upsert_global_server(&def)?;
    }
    Ok(ResolveStatus::Applied)
}

fn with_locked_state<R>(f: impl FnOnce(&mut McpState) -> R) -> Result<R> {
    let path = crate::session::get_app_dir()?.join("mcp_state.json");
    crate::session::storage::locked_update(
        &path,
        |content| serde_json::from_str(content).context("parsing mcp_state.json"),
        |state| Ok(serde_json::to_string_pretty(state)?),
        |state| Ok::<_, anyhow::Error>(f(state)),
    )?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::mcp::mcp_model::load_global_mcp_servers;
    use crate::session::mcp::project_mcp::{parse_standard_mcp_servers, ProjectMcpTransport};

    fn read(json: &str) -> NativeRead {
        NativeRead {
            servers: parse_standard_mcp_servers(json).unwrap(),
            disabled_names: Default::default(),
            skipped: Vec::new(),
        }
    }

    fn fs(command: &str) -> NativeRead {
        read(&format!(
            r#"{{ "mcpServers": {{ "fs": {{ "command": "{command}" }} }} }}"#
        ))
    }

    fn command(server: &ProjectMcpServer) -> &str {
        match &server.transport {
            ProjectMcpTransport::Stdio { command, .. } => command,
            other => panic!("expected stdio, got {other:?}"),
        }
    }

    fn global_commands() -> Vec<String> {
        let app_dir = crate::session::get_app_dir().unwrap();
        load_global_mcp_servers(&app_dir)
            .unwrap()
            .iter()
            .map(|s| command(s).to_string())
            .collect()
    }

    fn make_conflict() -> McpConflict {
        reconcile_agent("claude", &fs("old")).unwrap();
        let r = reconcile_agent("claude", &fs("new")).unwrap();
        assert_eq!(r.conflicts.len(), 1);
        r.conflicts.into_iter().next().unwrap()
    }

    #[test]
    #[serial_test::serial]
    fn reconcile_adopts_new_and_persists_conflicts_and_removals() {
        let _home = crate::session::test_support::isolate_app_dir();
        let both = r#"{ "mcpServers": { "fs": { "command": "c" }, "gone": { "command": "g" } } }"#;
        for _ in 0..2 {
            let r = reconcile_agent("codex", &read(both)).unwrap();
            assert_eq!(r, McpReconcile::default(), "first open adopts silently");
        }
        for _ in 0..2 {
            let r = reconcile_agent("codex", &fs("c")).unwrap();
            let removed: Vec<_> = r.removed.iter().map(|s| s.name.as_str()).collect();
            assert_eq!(removed, vec!["gone"], "removal persists until dropped");
        }

        let _ = make_conflict();
        let r = reconcile_agent("claude", &fs("new")).unwrap();
        assert_eq!(r.conflicts.len(), 1, "conflict persists until resolved");
        let c = &r.conflicts[0];
        assert_eq!(
            (c.agent.as_str(), command(&c.previous), command(&c.current)),
            ("claude", "old", "new")
        );

        // A skipped entry must not report "fs" as removed.
        let poisoned = NativeRead {
            servers: Vec::new(),
            disabled_names: Default::default(),
            skipped: vec!["fs".to_string()],
        };
        let r = reconcile_agent("claude", &poisoned).unwrap();
        assert!(r.paused && r.removed.is_empty() && r.conflicts.is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn resolve_conflict_outcomes() {
        for (winner, token_ok, global) in [
            (ConflictWinner::Aoe, true, vec!["old"]),
            (ConflictWinner::Native, true, vec![]),
            (ConflictWinner::Aoe, false, vec![]),
        ] {
            let _home = crate::session::test_support::isolate_app_dir();
            let conflict = make_conflict();
            let token = if token_ok {
                conflict.fingerprint()
            } else {
                "not-the-real-fingerprint".into()
            };
            let expected = if token_ok {
                ResolveStatus::Applied
            } else {
                ResolveStatus::Stale
            };
            assert_eq!(
                resolve_conflict(&conflict, winner, &token).unwrap(),
                expected
            );
            assert_eq!(global_commands(), global, "{winner:?} {token_ok}");
            let remaining = reconcile_agent("claude", &fs("new")).unwrap().conflicts;
            assert_eq!(remaining.len(), usize::from(!token_ok));
        }
    }

    #[test]
    #[serial_test::serial]
    fn resolve_conflict_stale_when_native_changes_after_token_captured() {
        let _home = crate::session::test_support::isolate_app_dir();
        let token = make_conflict().fingerprint();
        let newer = reconcile_agent("claude", &fs("newer"))
            .unwrap()
            .conflicts
            .remove(0);
        assert_eq!(
            resolve_conflict(&newer, ConflictWinner::Aoe, &token).unwrap(),
            ResolveStatus::Stale
        );
        assert!(global_commands().is_empty());
    }
}
