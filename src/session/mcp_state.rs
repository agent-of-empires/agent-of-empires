//! Drift store for the unified MCP surface (#1996).
//!
//! AoE keeps no live daemon state for MCP servers, so "what AoE last knew about
//! a server" must be persisted. `<app_dir>/mcp_state.json` records, per agent,
//! the last-seen definition of every server read from that agent's native
//! config. On each surface open, the current native config is reconciled against
//! this snapshot to detect drift:
//!
//! - a server whose definition CHANGED since the snapshot is a conflict the user
//!   resolves (feature C);
//! - a server that DISAPPEARED from the native config is kept in AoE's view and
//!   flagged (keep-on-removal, feature D), rather than silently dropped;
//! - a server that is NEW (present in native, absent from the snapshot) is
//!   adopted silently and recorded, so a first-ever open raises zero conflicts.
//!
//! The store holds the FULL, unredacted definition (env and header values
//! included): keep-on-removal and "AoE wins" must be able to reconstruct a
//! working server, which a redacted snapshot or a bare fingerprint cannot. The
//! file therefore carries the same secrets the user already keeps in plaintext
//! in `mcp.json` and the agents' own configs; it is written owner-only and
//! redacted at every DISPLAY edge (see [`super::mcp_model::RedactedMcpServer`]),
//! never on disk. AoE writes only this store and its own `mcp.json`; it never
//! writes back to an agent-native config (sync is native -> AoE only).
//!
//! Concurrent surface opens serialize through an exclusive file lock, mirroring
//! the repo trust store (`repo_config::trust_repo`).

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::mcp_model::NativeRead;
use super::project_mcp::ProjectMcpServer;

/// On-disk shape of `<app_dir>/mcp_state.json`. A missing file is an empty
/// state. New optional file (no existing data shape changes), so no migration:
/// absence is the default and older binaries simply never read it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct McpState {
    /// Per-agent last-seen native snapshot: agent key -> (server name -> def).
    #[serde(default)]
    native_snapshots: BTreeMap<String, BTreeMap<String, ProjectMcpServer>>,
}

/// A server whose agent-native definition diverged from AoE's last-seen
/// snapshot. The user resolves which side wins (feature C); AoE never writes the
/// native file, so resolving "AoE wins" persists into the global `mcp.json`
/// instead (via the override writer added for keep/resolve actions).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConflict {
    pub agent: String,
    /// AoE's last-seen definition (the snapshot side).
    pub previous: ProjectMcpServer,
    /// What the native config holds right now (the native side).
    pub current: ProjectMcpServer,
}

impl McpConflict {
    /// Optimistic-concurrency token binding BOTH sides of the conflict as this
    /// surface saw them: the AoE snapshot side (`previous`) and the native side
    /// (`current`). [`resolve_conflict`] rejects the resolution as stale if this
    /// token no longer matches the freshly reconciled conflict, so a change to
    /// EITHER side after the surface captured the token, another surface
    /// re-baselining the snapshot, or the native config changing again, is caught
    /// rather than silently applying the user's old decision to new state.
    pub fn fingerprint(&self) -> String {
        super::project_mcp::fingerprint(&[self.previous.clone(), self.current.clone()])
    }
}

/// Which side wins a conflict resolution (feature C).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictWinner {
    /// Keep AoE's last-seen definition: promote it into the global `mcp.json`
    /// (where it outranks native) so it keeps forwarding unchanged.
    Aoe,
    /// Accept the native config's current definition: just re-baseline the
    /// snapshot to it. The native definition then forwards on its own.
    Native,
}

/// Outcome of a conflict resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveStatus {
    /// The resolution was applied.
    Applied,
    /// The on-disk snapshot changed since the surface opened the modal (another
    /// surface resolved this conflict first); nothing was changed. The caller
    /// should refetch and re-prompt rather than blindly overwrite.
    Stale,
}

/// Outcome of reconciling one agent's native config against the snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpReconcile {
    /// Servers whose definition changed since the snapshot: conflicts (C).
    pub conflicts: Vec<McpConflict>,
    /// Servers gone from the native config since the snapshot, kept in AoE's
    /// view rather than silently dropped (keep-on-removal, D). The full last-seen
    /// definition, so the surface can still show (and the user can still keep)
    /// the server.
    pub removed: Vec<ProjectMcpServer>,
    /// True when drift detection was PAUSED for this agent because the native
    /// read skipped a malformed entry: a server that failed to parse must not be
    /// reported as "removed" (it is still in the file, just unreadable), so the
    /// snapshot is left untouched and no conflicts/removals are reported.
    pub paused: bool,
}

/// Path to the drift store, shared across all profiles (a server's drift is a
/// property of the host config, not of a session profile).
fn mcp_state_path() -> Result<PathBuf> {
    Ok(super::get_app_dir()?.join("mcp_state.json"))
}

/// Reconcile one agent's CURRENT native read against the stored snapshot,
/// updating the snapshot and returning the drift the surface must show.
///
/// Write policy: NEW servers are adopted into the snapshot immediately (so the
/// next open does not re-report them), and unchanged servers stay. Conflicting
/// and removed servers KEEP their old snapshot value (the AoE side), pending an
/// explicit user resolution, so the same drift surfaces on every open until the
/// user acts. If the native read skipped a malformed entry, drift detection is
/// paused and the snapshot is left completely untouched.
///
/// The whole read-modify-write runs under an exclusive lock so concurrent
/// surface opens (e.g. web and TUI) cannot clobber each other's snapshot.
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

    let current: BTreeMap<String, ProjectMcpServer> = read
        .servers
        .iter()
        .map(|s| (s.name.clone(), s.clone()))
        .collect();

    with_locked_state(|state| {
        let snapshot = state.native_snapshots.entry(agent.to_string()).or_default();

        let mut conflicts = Vec::new();
        for (name, cur) in &current {
            if let Some(prev) = snapshot.get(name) {
                if prev != cur {
                    conflicts.push(McpConflict {
                        agent: agent.to_string(),
                        previous: prev.clone(),
                        current: cur.clone(),
                    });
                }
            }
        }

        let removed: Vec<ProjectMcpServer> = snapshot
            .iter()
            .filter(|(name, _)| !current.contains_key(*name))
            .map(|(_, def)| def.clone())
            .collect();

        // Adopt new servers (present in native, absent from snapshot). Unchanged
        // servers already match. Conflicts and removals deliberately keep their
        // old snapshot value until the user resolves them.
        for (name, cur) in &current {
            snapshot.entry(name.clone()).or_insert_with(|| cur.clone());
        }

        McpReconcile {
            conflicts,
            removed,
            paused: false,
        }
    })
}

/// Keep a server that was removed from a native config (feature D): promote its
/// last-seen definition (held in the snapshot) into the global `mcp.json` so it
/// keeps forwarding as `global`, then forget the snapshot entry so it stops
/// being reported as kept-on-removal. By NAME so every surface can call it
/// (the web client never holds the unredacted definition). Returns `false` if
/// no such kept entry exists (already kept or dropped by another surface).
/// Reads the definition, promotes, then forgets, so a failed global write
/// leaves the entry intact and still keepable.
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

/// Drop a server from an agent's snapshot. Finalizes a keep-on-removal "drop"
/// decision (or the second half of [`keep_removed`]). The server stops being
/// reported as kept-on-removal on the next open. A no-op if already gone.
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

/// Resolve a conflict between AoE's snapshot and the native config (feature C).
///
/// `expected_fingerprint` is the optimistic-concurrency token the surface
/// captured when it opened the modal ([`McpConflict::fingerprint`]), binding
/// BOTH the snapshot (`previous`) and native (`current`) sides. The caller is
/// expected to pass a freshly reconciled `conflict`; if its token no longer
/// matches `expected_fingerprint`, either side moved since the surface captured
/// it (the native config changed again, or another surface re-baselined the
/// snapshot), so nothing changes and [`ResolveStatus::Stale`] is returned. Under
/// the store lock this is re-checked against the on-disk snapshot to close the
/// window between the caller's reconcile and this write. Otherwise the snapshot
/// is re-baselined to the native definition (so the conflict does not
/// re-surface), and for [`ConflictWinner::Aoe`] the AoE-side definition is
/// additionally promoted into the global `mcp.json`. AoE never writes the native
/// config either way.
pub fn resolve_conflict(
    conflict: &McpConflict,
    winner: ConflictWinner,
    expected_fingerprint: &str,
) -> Result<ResolveStatus> {
    let name = conflict.current.name.clone();

    // The token binds both sides of the conflict as the surface saw them.
    // Recompute from the freshly reconciled conflict the caller passed: if
    // either side moved since the token was captured, it no longer matches and
    // the resolution is stale (the user's decision was made against old state).
    if conflict.fingerprint() != expected_fingerprint {
        return Ok(ResolveStatus::Stale);
    }

    // Phase 1, under the store lock: re-verify the snapshot still holds the
    // previous side the caller resolved against (closing the window between the
    // caller's reconcile and this write), re-baseline it, and report whether the
    // AoE side must still be promoted to global.
    enum Decision {
        Stale,
        Applied { promote: Option<ProjectMcpServer> },
    }
    let decision = with_locked_state(|state| {
        let Some(snap) = state
            .native_snapshots
            .get(&conflict.agent)
            .and_then(|m| m.get(&name))
        else {
            return Decision::Stale;
        };
        if snap != &conflict.previous {
            return Decision::Stale;
        }
        let promote = match winner {
            ConflictWinner::Aoe => Some(snap.clone()),
            ConflictWinner::Native => None,
        };
        // Re-baseline to the native definition so subsequent diffs compare
        // against the now-known state and the conflict does not re-surface.
        state
            .native_snapshots
            .get_mut(&conflict.agent)
            .expect("snapshot present: looked up above under the same lock")
            .insert(name.clone(), conflict.current.clone());
        Decision::Applied { promote }
    })?;

    match decision {
        Decision::Stale => Ok(ResolveStatus::Stale),
        Decision::Applied { promote } => {
            // Promote AoE's definition into the global mcp.json (a separate
            // locked file) only after the snapshot re-baseline committed, so a
            // stale resolution never writes global.
            if let Some(def) = promote {
                super::mcp_overrides::upsert_global_server(&def)?;
            }
            Ok(ResolveStatus::Applied)
        }
    }
}

/// Parse the drift store under an exclusive sidecar lock, hand it to `f`, then
/// persist the (possibly mutated) state atomically while still holding the
/// lock. The lock serializes concurrent surface opens (web and TUI) so neither
/// clobbers the other's snapshot; `locked_update` also keeps the store
/// owner-only, which matters here because it holds the same plaintext secrets
/// as the user's mcp.json and native configs.
fn with_locked_state<R>(f: impl FnOnce(&mut McpState) -> R) -> Result<R> {
    let path = mcp_state_path()?;
    super::storage::locked_update(
        &path,
        |content| serde_json::from_str(content).context("parsing mcp_state.json"),
        |state| Ok(serde_json::to_string_pretty(state)?),
        |state| Ok::<_, anyhow::Error>(f(state)),
    )?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::project_mcp::parse_standard_mcp_servers;

    /// The drift store path derives from HOME; the env mutation is serialized
    /// across the whole suite by `#[serial_test::serial]` on each test (the same
    /// global lock the supervisor's HOME-touching tests use), and the returned
    /// `TempDir` must be kept alive for the test body.
    fn set_tmp_home() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: serialized by `#[serial]`; matches the existing pattern.
        unsafe {
            std::env::set_var("HOME", dir.path());
            std::env::set_var("XDG_CONFIG_HOME", dir.path().join(".config"));
        }
        dir
    }

    fn read(json: &str) -> NativeRead {
        NativeRead {
            servers: parse_standard_mcp_servers(json).unwrap(),
            skipped: Vec::new(),
        }
    }

    #[test]
    #[serial_test::serial]
    fn first_open_adopts_silently_no_conflicts() {
        let _home = set_tmp_home();
        let r = reconcile_agent(
            "claude",
            &read(r#"{ "mcpServers": { "fs": { "command": "c" }, "remote": { "type": "http", "url": "u" } } }"#),
        )
        .unwrap();
        assert!(r.conflicts.is_empty());
        assert!(r.removed.is_empty());
        assert!(!r.paused);

        // Second open against the same native set sees no drift (adopted).
        let r2 = reconcile_agent(
            "claude",
            &read(r#"{ "mcpServers": { "fs": { "command": "c" }, "remote": { "type": "http", "url": "u" } } }"#),
        )
        .unwrap();
        assert!(r2.conflicts.is_empty() && r2.removed.is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn changed_definition_is_conflict_and_snapshot_holds_old() {
        let _home = set_tmp_home();
        reconcile_agent(
            "claude",
            &read(r#"{ "mcpServers": { "fs": { "command": "old" } } }"#),
        )
        .unwrap();
        let r = reconcile_agent(
            "claude",
            &read(r#"{ "mcpServers": { "fs": { "command": "new" } } }"#),
        )
        .unwrap();
        assert_eq!(r.conflicts.len(), 1);
        let c = &r.conflicts[0];
        assert_eq!(c.agent, "claude");
        // previous = snapshot (old), current = native (new).
        assert!(matches!(&c.previous.transport,
            crate::session::project_mcp::ProjectMcpTransport::Stdio { command, .. } if command == "old"));
        assert!(matches!(&c.current.transport,
            crate::session::project_mcp::ProjectMcpTransport::Stdio { command, .. } if command == "new"));

        // Unresolved conflict re-surfaces on the next open (snapshot kept old).
        let r2 = reconcile_agent(
            "claude",
            &read(r#"{ "mcpServers": { "fs": { "command": "new" } } }"#),
        )
        .unwrap();
        assert_eq!(r2.conflicts.len(), 1, "conflict persists until resolved");
    }

    #[test]
    #[serial_test::serial]
    fn disappeared_server_is_kept_on_removal() {
        let _home = set_tmp_home();
        reconcile_agent(
            "claude",
            &read(r#"{ "mcpServers": { "fs": { "command": "c" }, "gone": { "command": "g" } } }"#),
        )
        .unwrap();
        let r = reconcile_agent(
            "claude",
            &read(r#"{ "mcpServers": { "fs": { "command": "c" } } }"#),
        )
        .unwrap();
        assert_eq!(r.removed.len(), 1);
        assert_eq!(r.removed[0].name, "gone");

        // Still flagged on the next open (snapshot keeps the removed entry).
        let r2 = reconcile_agent(
            "claude",
            &read(r#"{ "mcpServers": { "fs": { "command": "c" } } }"#),
        )
        .unwrap();
        assert_eq!(r2.removed.len(), 1, "removal persists until dropped");
    }

    fn make_conflict(agent: &str) -> McpConflict {
        reconcile_agent(
            agent,
            &read(r#"{ "mcpServers": { "fs": { "command": "old" } } }"#),
        )
        .unwrap();
        let r = reconcile_agent(
            agent,
            &read(r#"{ "mcpServers": { "fs": { "command": "new" } } }"#),
        )
        .unwrap();
        assert_eq!(r.conflicts.len(), 1);
        r.conflicts.into_iter().next().unwrap()
    }

    #[test]
    #[serial_test::serial]
    fn resolve_conflict_aoe_wins_promotes_to_global_and_clears() {
        let _home = set_tmp_home();
        let conflict = make_conflict("claude");
        let fp = conflict.fingerprint();
        let status = resolve_conflict(&conflict, ConflictWinner::Aoe, &fp).unwrap();
        assert_eq!(status, ResolveStatus::Applied);

        // AoE's old definition is promoted into global mcp.json.
        let app_dir = crate::session::get_app_dir().unwrap();
        let global = crate::session::mcp_model::load_global_mcp_servers(&app_dir).unwrap();
        assert_eq!(global.len(), 1);
        assert!(matches!(&global[0].transport,
            crate::session::project_mcp::ProjectMcpTransport::Stdio { command, .. } if command == "old"));

        // Conflict no longer surfaces (snapshot re-baselined to native).
        let r = reconcile_agent(
            "claude",
            &read(r#"{ "mcpServers": { "fs": { "command": "new" } } }"#),
        )
        .unwrap();
        assert!(r.conflicts.is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn resolve_conflict_native_wins_clears_without_global_write() {
        let _home = set_tmp_home();
        let conflict = make_conflict("claude");
        let fp = conflict.fingerprint();
        assert_eq!(
            resolve_conflict(&conflict, ConflictWinner::Native, &fp).unwrap(),
            ResolveStatus::Applied
        );
        let app_dir = crate::session::get_app_dir().unwrap();
        let global = crate::session::mcp_model::load_global_mcp_servers(&app_dir).unwrap();
        assert!(global.is_empty(), "native wins must not write global");
        let r = reconcile_agent(
            "claude",
            &read(r#"{ "mcpServers": { "fs": { "command": "new" } } }"#),
        )
        .unwrap();
        assert!(r.conflicts.is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn resolve_conflict_stale_token_is_rejected() {
        let _home = set_tmp_home();
        let conflict = make_conflict("claude");
        let status =
            resolve_conflict(&conflict, ConflictWinner::Aoe, "not-the-real-fingerprint").unwrap();
        assert_eq!(status, ResolveStatus::Stale);

        // Nothing changed: conflict still surfaces, global untouched.
        let app_dir = crate::session::get_app_dir().unwrap();
        assert!(crate::session::mcp_model::load_global_mcp_servers(&app_dir)
            .unwrap()
            .is_empty());
        let r = reconcile_agent(
            "claude",
            &read(r#"{ "mcpServers": { "fs": { "command": "new" } } }"#),
        )
        .unwrap();
        assert_eq!(
            r.conflicts.len(),
            1,
            "stale resolution must not clear the conflict"
        );
    }

    #[test]
    #[serial_test::serial]
    fn resolve_conflict_stale_when_native_changes_after_token_captured() {
        let _home = set_tmp_home();
        // Surface sees old->new and captures a token binding both sides.
        let conflict = make_conflict("claude");
        let token = conflict.fingerprint();

        // The native config then changes AGAIN (new -> newer) before the user's
        // resolution lands; the caller reconciles and finds the newer conflict.
        let r = reconcile_agent(
            "claude",
            &read(r#"{ "mcpServers": { "fs": { "command": "newer" } } }"#),
        )
        .unwrap();
        let newer = r.conflicts.into_iter().next().unwrap();

        // Applying the stale token against the newer native definition is
        // rejected, and nothing is promoted to global.
        let status = resolve_conflict(&newer, ConflictWinner::Aoe, &token).unwrap();
        assert_eq!(status, ResolveStatus::Stale);
        let app_dir = crate::session::get_app_dir().unwrap();
        assert!(crate::session::mcp_model::load_global_mcp_servers(&app_dir)
            .unwrap()
            .is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn skipped_entry_pauses_drift_detection() {
        let _home = set_tmp_home();
        reconcile_agent(
            "claude",
            &read(r#"{ "mcpServers": { "fs": { "command": "c" } } }"#),
        )
        .unwrap();
        // A read with a skipped (malformed) entry must not report "fs" removed.
        let poisoned = NativeRead {
            servers: Vec::new(),
            skipped: vec!["fs".to_string()],
        };
        let r = reconcile_agent("claude", &poisoned).unwrap();
        assert!(r.paused);
        assert!(
            r.removed.is_empty(),
            "paused detection must not report removals"
        );
        assert!(r.conflicts.is_empty());
    }
}
