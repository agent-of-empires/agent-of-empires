use std::fmt;

use serde::{Deserialize, Serialize};

/// Identifier of a plugin, e.g. `aoe.status` or `someuser.review-helper`.
///
/// Lowercase ASCII segments separated by dots; segments may contain digits and
/// hyphens but must start with a letter. The id namespaces everything the
/// plugin touches: its config table (`[plugins."<id>"]`), its `plugin_meta`
/// slot on sessions, its event topics (`plugin.<id>.*`), and its canonical
/// action names (`plugin.<id>.<action>`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PluginId(String);

/// Rejection reason for a malformed plugin id.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid plugin id {id:?}: {reason}")]
#[non_exhaustive]
pub struct InvalidPluginId {
    pub id: String,
    pub reason: &'static str,
}

impl PluginId {
    pub fn new(id: impl Into<String>) -> Result<Self, InvalidPluginId> {
        let id = id.into();
        let reject = |reason| {
            Err(InvalidPluginId {
                id: id.clone(),
                reason,
            })
        };
        if id.is_empty() {
            return reject("empty");
        }
        if id.len() > 64 {
            return reject("longer than 64 bytes");
        }
        for segment in id.split('.') {
            let mut chars = segment.chars();
            match chars.next() {
                Some(c) if c.is_ascii_lowercase() => {}
                _ => {
                    return reject("each dot-separated segment must start with a lowercase letter")
                }
            }
            if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
                return reject("segments may only contain lowercase letters, digits, and hyphens");
            }
        }
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this id sits in a namespace reserved for first-party plugins:
    /// `aoe.*` (bundled builtins) and `agent-of-empires.*` (official plugins
    /// shipped through the featured index). The host lets a community install
    /// use a reserved namespace only when the source is featured-verified, so
    /// a third party cannot publish as `aoe.web` or `agent-of-empires.github`
    /// and usurp the builtin/official id, the telemetry allowlist, or the
    /// `plugin_meta` namespace. Builtin manifests are loaded from inside the
    /// binary and never pass through that install gate.
    pub fn is_reserved_namespace(&self) -> bool {
        matches!(
            self.0.split('.').next(),
            Some("aoe") | Some("agent-of-empires")
        )
    }
}

impl fmt::Display for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for PluginId {
    type Error = InvalidPluginId;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<PluginId> for String {
    fn from(value: PluginId) -> Self {
        value.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_dotted_lowercase_ids() {
        for ok in ["aoe.status", "a", "someuser.review-helper", "x.y2.z-3"] {
            assert!(PluginId::new(ok).is_ok(), "{ok} should be valid");
        }
    }

    #[test]
    fn rejects_malformed_ids() {
        for bad in [
            "",
            "Aoe.status",
            "aoe..status",
            "aoe.2fast",
            "-x",
            "aoe.st_at",
            "aoe.st at",
        ] {
            assert!(PluginId::new(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn reserved_namespace_policy_is_pinned() {
        for reserved in ["aoe.status", "aoe.web", "agent-of-empires.github"] {
            assert!(
                PluginId::new(reserved).unwrap().is_reserved_namespace(),
                "{reserved} should be reserved"
            );
        }
        for open in ["someuser.review-helper", "acme.review", "aoextra.thing"] {
            assert!(
                !PluginId::new(open).unwrap().is_reserved_namespace(),
                "{open} should be open"
            );
        }
    }

    #[test]
    fn serde_round_trips_and_validates() {
        let id: PluginId = serde_json::from_str("\"aoe.status\"").unwrap();
        assert_eq!(id.as_str(), "aoe.status");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"aoe.status\"");
        assert!(serde_json::from_str::<PluginId>("\"Not Valid\"").is_err());
    }
}
