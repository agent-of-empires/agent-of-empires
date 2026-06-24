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
