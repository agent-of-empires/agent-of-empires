//! Manifest-driven pane status detection: each agent's rules
//! (`manifests/<agent>.toml`) name a state, a screen [`region`], a priority and
//! a matcher; the highest matching priority wins. The hook file is a rule too.

mod adapter;
mod manifest;
mod omp;
mod region;

use std::collections::HashMap;
use std::sync::LazyLock;

use crate::session::Status;
use adapter::Adapter;
pub use adapter::{CapturePolicy, CaptureScope};
pub use manifest::HookObservation;
use manifest::Manifest;
use region::Rows;

const MANIFEST_SOURCES: &[(&str, &str, &Adapter)] = &[
    (
        "claude",
        include_str!("manifests/claude.toml"),
        &adapter::PLAIN,
    ),
    (
        "cursor",
        include_str!("manifests/cursor.toml"),
        &adapter::PLAIN,
    ),
    (
        "opencode",
        include_str!("manifests/opencode.toml"),
        &adapter::PLAIN,
    ),
    ("vibe", include_str!("manifests/vibe.toml"), &adapter::PLAIN),
    (
        "droid",
        include_str!("manifests/droid.toml"),
        &adapter::PLAIN,
    ),
    (
        "gemini",
        include_str!("manifests/gemini.toml"),
        &adapter::PLAIN,
    ),
    ("qwen", include_str!("manifests/qwen.toml"), &adapter::PLAIN),
    (
        "copilot",
        include_str!("manifests/copilot.toml"),
        &adapter::PLAIN,
    ),
    (
        "antigravity",
        include_str!("manifests/antigravity.toml"),
        &adapter::PLAIN,
    ),
    (
        "hermes",
        include_str!("manifests/hermes.toml"),
        &adapter::PLAIN,
    ),
    ("pi", include_str!("manifests/pi.toml"), &adapter::PLAIN),
    (
        "codex",
        include_str!("manifests/codex.toml"),
        &adapter::PLAIN,
    ),
    ("omp", include_str!("manifests/omp.toml"), &omp::ADAPTER),
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

/// Raw facts accompanying a pane capture. Configured rules do not depend on them.
pub struct Observation<'a> {
    title: &'a str,
    hook: Option<HookObservation>,
    live_owned: bool,
}

impl<'a> Observation<'a> {
    pub fn new(title: &'a str, hook: Option<HookObservation>, live_owned: bool) -> Self {
        Self {
            title,
            hook,
            live_owned,
        }
    }
}

/// Adapter-defined semantic identity of non-screen evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservationKey {
    detector: &'static str,
    token: u64,
}

pub(super) struct Detector {
    manifest: Manifest,
    adapter: &'static Adapter,
}

impl Detector {
    pub(super) fn capture_policy(&self) -> CapturePolicy {
        self.adapter.capture
    }

    pub(super) fn observation_key(
        &'static self,
        observation: &Observation<'_>,
    ) -> Option<ObservationKey> {
        (self.adapter.observation_token)(observation).map(|token| ObservationKey {
            detector: &self.manifest.id,
            token,
        })
    }

    pub(super) fn detect(&'static self, screen: &str, observation: &Observation<'_>) -> Detection {
        let Some(rule) = (self.adapter.evaluate)(&self.manifest, Rows::new(screen), observation)
        else {
            return Detection::idle_by_default();
        };
        tracing::trace!(target: "tmux.status", "{} detection: rule {} matched", self.manifest.id, rule.id);
        Detection {
            status: (!rule.skip_state_update).then_some(rule.state).flatten(),
            visible: rule.visible,
            rule: rule.id.as_str(),
        }
    }
}

static MANIFESTS: LazyLock<HashMap<&'static str, Detector>> = LazyLock::new(|| {
    let mut map = HashMap::new();
    for (agent, source, adapter) in MANIFEST_SOURCES {
        match Manifest::parse(source, adapter.evidence_names) {
            Ok(manifest) => {
                debug_assert_eq!(&manifest.id, agent, "manifest id must match its file name");
                map.insert(*agent, Detector { manifest, adapter });
            }
            // Unreachable in a tested build (`manifests_compile`).
            Err(e) => tracing::error!(target: "tmux.status",
                "detection manifest for {agent} failed to compile, \
                 falling back to hookless idle: {e}"),
        }
    }
    map
});

pub(super) fn detector(agent: &str) -> Option<&'static Detector> {
    MANIFESTS.get(agent)
}

pub(super) fn plain_capture_policy() -> CapturePolicy {
    adapter::PLAIN.capture
}

pub fn has_manifest(agent: &str) -> bool {
    detector(agent).is_some()
}

/// Evaluate an agent manifest over an ANSI-stripped capture.
pub fn detect(agent: &str, screen: &str, observation: &Observation<'_>) -> Option<Detection> {
    Some(detector(agent)?.detect(screen, observation))
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
    let detector = detector(agent).expect("agent has a manifest");
    let observation = Observation::new(osc_title, hook, true);
    (detector.adapter.rule_matches)(&detector.manifest, rule_id, Rows::new(screen), &observation)
}

#[cfg(test)]
pub(crate) fn rule_max_age(agent: &str, rule_id: &str) -> Option<std::time::Duration> {
    detector(agent)
        .expect("agent has a manifest")
        .manifest
        .rule(rule_id)
        .expect("rule exists")
        .max_age
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifests_compile() {
        for (agent, source, adapter) in MANIFEST_SOURCES {
            Manifest::parse(source, adapter.evidence_names)
                .unwrap_or_else(|e| panic!("manifest {agent} failed to compile: {e}"));
        }
        assert_eq!(MANIFESTS.len(), MANIFEST_SOURCES.len());
    }
}
