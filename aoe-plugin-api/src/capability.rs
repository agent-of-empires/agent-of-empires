//! Capability taxonomy and trust levels for the plugin system.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A capability a plugin requests in its manifest `capabilities = [...]` array.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustLevel {
    Builtin,
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
