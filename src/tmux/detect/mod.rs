//! Manifest-driven pane status detection.
//!
//! Each agent's detection is a table of rules (`manifests/<agent>.toml`)
//! rather than a hand-written chain of checks: a rule names its state, the
//! screen [`region`] its evidence appears in, a priority, and a matcher.
//! Highest matching priority wins.
//!
//! The hook file is a rule like any other, so its authority is declared next
//! to the screen rules it competes with. A blocking prompt on screen outranks
//! a `running` write, and a `running` write carries a freshness bound, so a
//! lost terminating hook cannot pin a parked session on Running.

mod manifest;
mod region;

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::session::Status;
pub use manifest::HookObservation;
use manifest::Manifest;
use region::Screen;

/// Rule sources, embedded so a build carries its detection data. TOML is
/// exempt from the `flake.nix` embedded-asset list, so these need no entry.
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
    /// `None` when the screen is an agent-owned viewer: the last known status
    /// stands rather than being overwritten by what the pager happens to show.
    pub status: Option<Status>,
    /// The matching rule read this state off the agent's own live chrome, so
    /// it is worth publishing without waiting for a confirming poll.
    pub visible: bool,
    /// The rule that decided, for the status-change log.
    pub rule: &'static str,
}

impl Detection {
    /// Nothing matched: no evidence of work or of a prompt.
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
                // Unreachable in a tested build (`manifests_compile` covers
                // every embedded file); losing one agent's rules to pane
                // detection is better than refusing to start.
                Err(e) => tracing::error!(target: "tmux.status",
                    "detection manifest for {agent} failed to compile, \
                     falling back to hookless idle: {e}"),
            }
        }
        map
    })
}

/// Whether `agent` is detected from a manifest rather than a hand-written
/// detector.
pub fn has_manifest(agent: &str) -> bool {
    manifests().contains_key(agent)
}

/// Evaluate `agent`'s manifest against one capture.
///
/// `screen` must already be ANSI-stripped. `osc_title` is the terminal title
/// the agent published (tmux's `#{pane_title}`), empty when unknown; `hook` is
/// the session's status file, `None` when hooks are off or none has been
/// written yet.
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
        // A viewer rule carries no state: the caller holds what it had.
        status: (!rule.skip_state_update).then_some(rule.state).flatten(),
        visible: rule.visible,
        // Manifests are parsed once into a process-lifetime map, so the log
        // borrows the rule id rather than copying it per poll.
        rule: rule.id.as_str(),
    })
}

/// Whether one named rule matches, regardless of what else does. Fixture
/// tests assert the shape they claim to exercise is actually present, so a
/// fixture that stops carrying its signal fails loudly instead of passing on
/// some other rule.
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

/// A rule's declared freshness bound, so boundary tests derive it from the
/// manifest instead of restating the number.
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
