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
    use fs2::FileExt;
    use std::io::{Read, Seek, SeekFrom, Write};

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

    let path = mcp_state_path()?;
    if !path.exists() {
        std::fs::write(&path, "").with_context(|| format!("creating {}", path.display()))?;
    }
    // Owner-only: the store holds the same plaintext secrets as the user's
    // mcp.json and native configs, so it must never widen beyond the owner.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    let mut lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    lock_file
        .lock_exclusive()
        .context("locking mcp_state.json")?;

    let mut content = String::new();
    lock_file.read_to_string(&mut content)?;
    let mut state: McpState = if content.trim().is_empty() {
        McpState::default()
    } else {
        serde_json::from_str(&content).context("parsing mcp_state.json")?
    };

    let current: BTreeMap<String, ProjectMcpServer> = read
        .servers
        .iter()
        .map(|s| (s.name.clone(), s.clone()))
        .collect();
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
    // servers already match. Conflicts and removals deliberately keep their old
    // snapshot value until the user resolves them.
    for (name, cur) in &current {
        snapshot.entry(name.clone()).or_insert_with(|| cur.clone());
    }

    let new_content = serde_json::to_string_pretty(&state)?;
    lock_file.seek(SeekFrom::Start(0))?;
    lock_file.set_len(0)?;
    lock_file.write_all(new_content.as_bytes())?;

    Ok(McpReconcile {
        conflicts,
        removed,
        paused: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::project_mcp::parse_standard_mcp_servers;
    use std::sync::{Mutex, MutexGuard};

    // The drift store lives at a HOME-derived path; serialize the env mutation.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct TmpHome {
        _guard: MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
    }

    fn with_tmp_home() -> TmpHome {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: serialized by ENV_LOCK for the duration of the returned guard.
        unsafe {
            std::env::set_var("HOME", dir.path());
            std::env::set_var("XDG_CONFIG_HOME", dir.path().join(".config"));
        }
        TmpHome {
            _guard: guard,
            _dir: dir,
        }
    }

    fn read(json: &str) -> NativeRead {
        NativeRead {
            servers: parse_standard_mcp_servers(json).unwrap(),
            skipped: Vec::new(),
        }
    }

    #[test]
    fn first_open_adopts_silently_no_conflicts() {
        let _home = with_tmp_home();
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
    fn changed_definition_is_conflict_and_snapshot_holds_old() {
        let _home = with_tmp_home();
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
    fn disappeared_server_is_kept_on_removal() {
        let _home = with_tmp_home();
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

    #[test]
    fn skipped_entry_pauses_drift_detection() {
        let _home = with_tmp_home();
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
