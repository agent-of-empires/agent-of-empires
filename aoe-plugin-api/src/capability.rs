//! Capability taxonomy and trust levels for the plugin system.
//!
//! A capability gates runtime access to a resource that can affect user data,
//! host state, the OS, or the network. Static contributions (commands,
//! keybinds, themes, ui, status, panes) are NOT capabilities; they are plain
//! manifest sections that need no grant. A capability is what the one-time
//! install prompt asks the user to approve, and what a persisted grant is
//! pinned to.
//!
//! Capabilities are open strings rather than a closed enum so a follow-up issue
//! can introduce a new permission without bumping `api_version`. The host
//! validates a requested capability against [`KNOWN_CAPABILITIES`] at install
//! and grant time, rejecting an unknown one rather than silently granting it.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A capability a plugin requests in its manifest `capabilities = [...]` array.
///
/// Stored as a free string; [`CapabilityId::is_known`] reports whether this
/// host version recognizes it. The host never grants an unknown capability.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapabilityId(String);

impl CapabilityId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this host version recognizes the capability. An unknown
    /// capability is rejected at install (`unsupported capability; upgrade
    /// aoe`), never silently granted.
    pub fn is_known(&self) -> bool {
        KNOWN_CAPABILITIES.contains(&self.0.as_str())
    }
}

impl fmt::Display for CapabilityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for CapabilityId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

/// Resource/effect capabilities this host version understands.
///
/// Each gates a runtime resource that the worker (#2095) or a contribution
/// handler reaches. A plugin's own declared settings need no `config.*`:
/// `config.read` / `config.write` mean host/global or other-plugin
/// configuration, not the plugin's own table.
pub const KNOWN_CAPABILITIES: &[&str] = &[
    // Executing plugin code at all is materially different from loading static
    // metadata, so it is its own capability.
    "runtime.worker",
    // Reading and mutating the session the plugin is attached to.
    "session.read",
    "session.write",
    // Host/global or other-plugin configuration (NOT the plugin's own settings).
    "config.read",
    "config.write",
    // Spawning OS subprocesses beyond the plugin's own worker.
    "process.spawn",
    // Outbound network access.
    "net",
    // Filesystem access outside the plugin's own directory. Read is split from
    // write because the two carry very different risk.
    "fs.read",
    "fs.write",
    // Clipboard read is far more sensitive than write, so they are separate.
    "clipboard.read",
    "clipboard.write",
    // Posting desktop / TUI notifications.
    "notifications",
    // Opening an external URL in the user's browser, driven by a command's
    // `action` (or a future host RPC). Distinct from a rendered `href` anchor
    // the user clicks, which needs no grant.
    "browser_open",
    // Reading and mutating the active ACP composer draft through a
    // host-mediated composer action. The dashboard owns the actual draft state;
    // plugins only receive a click-scoped snapshot or request a validated edit.
    "composer.read",
    "composer.write",
    // Reading the ACP capability catalog: which agents exist and their
    // advertised structured-session models/modes. Read-only discovery; the host
    // never launches an agent to answer it.
    "acp.capabilities.read",
    // Triggering a handshake-only catalog probe: the host spawns the agent
    // adapter, runs initialize + session/new (no prompt turn, so no tokens),
    // records the advertised models/modes/thought-levels, and tears the process
    // down. Distinct from the read grant because it makes the host spawn a real
    // process (CPU, startup latency, possibly network auth), a different risk
    // axis than reading a cached catalog.
    "acp.capabilities.probe",
    // Creating a host-owned structured-view session (the host validates agent,
    // model, mode, and repository trust; the plugin cannot bypass those).
    "session.create",
    // Delivering a prompt/turn to a session the plugin created. Scoped to the
    // creating plugin; it is NOT a license to write to arbitrary user sessions.
    "session.prompt",
    // A distinct, high-severity grant required when `session.create` selects a
    // host-classified unattended (auto-approval) mode, i.e. the plugin may start
    // an agent and send it a prompt with no user present. Never implied by
    // `session.create` or `session.prompt`; repository trust still applies.
    "session.unattended",
];

/// How far a plugin is trusted. Host-assigned at load time, never declared in
/// the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustLevel {
    /// Compiled into the binary. Fully trusted: capabilities are auto-granted,
    /// no install prompt.
    Builtin,
    /// Installed from an external source (GitHub or a local dir). Untrusted:
    /// every requested capability must be granted by the user.
    Community,
}

impl TrustLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            TrustLevel::Builtin => "builtin",
            TrustLevel::Community => "community",
        }
    }
}

impl fmt::Display for TrustLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
