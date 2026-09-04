//! Capability taxonomy and trust levels for the plugin system.
//!
//! A capability gates runtime access to a resource that can affect user data,
//! host state, the OS, or the network. Static contributions (commands,
//! keybinds, themes, ui, status, panes) are NOT capabilities; they are plain
//! manifest sections that need no grant. A capability is what the one-time
//! install prompt asks the user to approve, and what a persisted grant is
//! pinned to.
//!
//! Capabilities are open strings so new permissions do not require an API
//! version bump. The host rejects strings it does not recognize.

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
/// Each gates a runtime resource a worker or contribution handler reaches. A
/// plugin's own declared settings need no `config.*`:
/// `config.read` / `config.write` mean host/global or other-plugin
/// configuration, not the plugin's own table.
pub const KNOWN_CAPABILITIES: &[&str] = &[
    "runtime.worker",
    "session.read",
    "session.write",
    "config.read",
    "config.write",
    "process.spawn",
    "net",
    "fs.read",
    "fs.write",
    "clipboard.read",
    "clipboard.write",
    "notifications",
    "browser_open",
    "composer.read",
    "composer.write",
    "acp.capabilities.read",
    "acp.capabilities.probe",
    "session.create",
    "session.prompt",
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
