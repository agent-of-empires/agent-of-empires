//! Manifest-driven pane status detection: each agent's rules
//! (`manifests/<agent>.toml`) name a state, a screen [`region`], a priority and
//! a matcher; the highest matching priority wins. The hook file is a rule too.

mod manifest;
mod region;

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::session::Status;
pub use manifest::HookObservation;
use manifest::Manifest;
use region::Screen;

const MANIFEST_SOURCES: &[(&str, &str)] = &[
    ("claude", include_str!("manifests/claude.toml")),
    ("cursor", include_str!("manifests/cursor.toml")),
    ("opencode", include_str!("manifests/opencode.toml")),
    ("vibe", include_str!("manifests/vibe.toml")),
    ("droid", include_str!("manifests/droid.toml")),
    ("gemini", include_str!("manifests/gemini.toml")),
    ("qwen", include_str!("manifests/qwen.toml")),
    ("copilot", include_str!("manifests/copilot.toml")),
    ("antigravity", include_str!("manifests/antigravity.toml")),
    ("hermes", include_str!("manifests/hermes.toml")),
    ("pi", include_str!("manifests/pi.toml")),
    ("codex", include_str!("manifests/codex.toml")),
    ("omp", include_str!("manifests/omp.toml")),
];

/// What one capture says about a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Detection {
    /// `None` for an agent-owned viewer: the last known status stands.
    pub status: Option<Status>,
    /// Read off the agent's live chrome, so it can publish without a second poll.
    pub visible: bool,
    pub rule: &'static str,
}

impl Detection {
    pub(crate) fn idle_by_default() -> Self {
        Self {
            status: Some(Status::Idle),
            visible: false,
            rule: "no_rule",
        }
    }
}

fn manifests() -> &'static HashMap<&'static str, Manifest> {
    static MANIFESTS: OnceLock<HashMap<&'static str, Manifest>> = OnceLock::new();
    MANIFESTS.get_or_init(|| {
        let mut map = HashMap::new();
        for (agent, source) in MANIFEST_SOURCES {
            match Manifest::parse(source) {
                Ok(m) => {
                    debug_assert_eq!(&m.id, agent, "manifest id must match its file name");
                    map.insert(*agent, m);
                }
                // Unreachable in a tested build (`manifests_compile`).
                Err(e) => tracing::error!(target: "tmux.status",
                    "detection manifest for {agent} failed to compile, \
                     falling back to hookless idle: {e}"),
            }
        }
        map
    })
}

pub fn has_manifest(agent: &str) -> bool {
    manifests().contains_key(agent)
}

/// Evaluate `agent`'s manifest. `screen` is ANSI-stripped; `osc_title` is empty
/// when unknown; `hook` is `None` without a status file.
pub fn detect(
    agent: &str,
    screen: &str,
    osc_title: &str,
    hook: Option<HookObservation>,
) -> Option<Detection> {
    let manifest = manifests().get(agent)?;
    let parsed = Screen::new(screen, osc_title);
    let Some(rule) = manifest.evaluate(&parsed, hook) else {
        return Some(Detection::idle_by_default());
    };
    tracing::trace!(target: "tmux.status", "{agent} detection: rule {} matched", rule.id);
    Some(Detection {
        status: (!rule.skip_state_update).then_some(rule.state).flatten(),
        visible: rule.visible,
        rule: rule.id.as_str(),
    })
}

/// Whether one named rule matches, so fixtures prove they carry their signal.
#[cfg(test)]
pub(crate) fn rule_matches(
    agent: &str,
    rule_id: &str,
    screen: &str,
    osc_title: &str,
    hook: Option<HookObservation>,
) -> bool {
    let manifest = manifests().get(agent).expect("agent has a manifest");
    let parsed = Screen::new(screen, osc_title);
    manifest.rule_matches(rule_id, &parsed, hook)
}

#[cfg(test)]
pub(crate) fn rule_max_age(agent: &str, rule_id: &str) -> Option<std::time::Duration> {
    manifests()
        .get(agent)
        .expect("agent has a manifest")
        .rule(rule_id)
        .expect("rule exists")
        .max_age
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifests_compile() {
        for (agent, source) in MANIFEST_SOURCES {
            Manifest::parse(source)
                .unwrap_or_else(|e| panic!("manifest {agent} failed to compile: {e}"));
        }
        assert_eq!(manifests().len(), MANIFEST_SOURCES.len());
    }

    #[test]
    fn unknown_agent_has_no_manifest() {
        assert!(!has_manifest("nonesuch"));
        assert!(detect("nonesuch", "anything", "", None).is_none());
    }
}
