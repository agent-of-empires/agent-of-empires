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
pub struct Detection<'a> {
    /// `None` for an agent-owned viewer: the last known status stands.
    pub status: Option<Status>,
    /// Read off the agent's live chrome, so it can publish without a second poll.
    pub visible: bool,
    pub rule: &'static str,
    /// Source text carried only by the selected Error rule's own evidence.
    pub diagnostic: Option<&'a str>,
}

impl Detection<'_> {
    pub(crate) fn idle_by_default() -> Self {
        Self {
            status: Some(Status::Idle),
            visible: false,
            rule: "no_rule",
            diagnostic: None,
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

    pub(super) fn detect<'a>(
        &'static self,
        screen: &'a str,
        observation: &Observation<'a>,
    ) -> Detection<'a> {
        let Some(evaluation) =
            (self.adapter.evaluate)(&self.manifest, Rows::new(screen), observation)
        else {
            return Detection::idle_by_default();
        };
        let rule = evaluation.rule;
        tracing::trace!(target: "tmux.status", "{} detection: rule {} matched", self.manifest.id, rule.id);
        Detection {
            status: (!rule.skip_state_update).then_some(rule.state).flatten(),
            visible: rule.visible,
            rule: rule.id.as_str(),
            diagnostic: evaluation.diagnostic,
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
pub fn detect<'a>(
    agent: &str,
    screen: &'a str,
    observation: &Observation<'a>,
) -> Option<Detection<'a>> {
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

    #[test]
    fn diagnostic_belongs_to_the_winning_error_region() {
        static ADAPTER: Adapter = Adapter {
            capture: CapturePolicy {
                scope: CaptureScope::Visible,
                fetch_missing_title: false,
                observe_ownership: false,
            },
            evidence_names: &["primary", "secondary"],
            observation_token: |_| None,
            evaluate: |manifest, rows, observation| {
                let evidence = [
                    region::Evidence {
                        text: rows.non_empty.first().copied().unwrap_or(""),
                        bottom_offset: 1,
                        diagnostic: rows.non_empty.first().copied(),
                    },
                    region::Evidence {
                        text: rows.non_empty.get(1).copied().unwrap_or(""),
                        bottom_offset: 0,
                        diagnostic: rows.non_empty.get(1).copied(),
                    },
                ];
                let screen = region::Screen::new(rows, observation.title, &evidence);
                manifest
                    .evaluate(&screen, observation.hook)
                    .map(|rule| adapter::Evaluation::new(rule, &evidence))
            },
            rule_matches: |_, _, _, _| unreachable!(),
        };
        static DETECTOR: LazyLock<Detector> = LazyLock::new(|| Detector {
            adapter: &ADAPTER,
            manifest: Manifest::parse(
                r#"
id = "diagnostics"
[[rules]]
id = "first_error"
state = "error"
priority = 700
region = "evidence(primary)"
contains = ["first error"]
[[rules]]
id = "second_error"
state = "error"
priority = 800
region = "evidence(secondary)"
contains = ["second error"]
[[rules]]
id = "active"
state = "running"
priority = 900
region = "evidence(secondary)"
contains = ["running"]
[[rules]]
id = "dialog"
state = "waiting"
priority = 900
region = "evidence(secondary)"
contains = ["waiting"]
[[rules]]
id = "other_error"
state = "error"
priority = 900
region = "whole_recent"
contains = ["other error"]
all = [{ region = "evidence(primary)", contains = ["first error"] }]
[[rules]]
id = "viewer"
state = "error"
skip_state_update = true
priority = 1000
region = "evidence(secondary)"
contains = ["viewer"]
"#,
                ADAPTER.evidence_names,
            )
            .unwrap(),
        });
        for (pane, status, diagnostic) in [
            (
                "first error\nquiet",
                Some(Status::Error),
                Some("first error"),
            ),
            (
                "first error\nsecond error",
                Some(Status::Error),
                Some("second error"),
            ),
            ("first error\nrunning", Some(Status::Running), None),
            ("first error\nwaiting", Some(Status::Waiting), None),
            ("first error\nother error", Some(Status::Error), None),
            ("first error\nviewer", None, None),
        ] {
            let detection = DETECTOR.detect(pane, &Observation::new("", None, true));
            assert_eq!(detection.status, status, "{pane}");
            assert_eq!(detection.diagnostic, diagnostic, "{pane}");
        }
    }
}
