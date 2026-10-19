//! Static surface adapters for manifest evaluation.

use super::manifest::{Manifest, Rule};
use super::region::{Rows, Screen};
use super::Observation;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureScope {
    Visible,
    History(usize),
}

impl CaptureScope {
    pub fn lines(self) -> usize {
        match self {
            Self::Visible => 0,
            Self::History(lines) => lines,
        }
    }

    /// A history capture is not interchangeable with the exact visible surface.
    pub fn can_reuse(self, captured_lines: usize) -> bool {
        match self {
            Self::Visible => captured_lines == 0,
            Self::History(lines) => captured_lines >= lines,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CapturePolicy {
    pub scope: CaptureScope,
    pub fetch_missing_title: bool,
    pub observe_ownership: bool,
}

type EvaluateSurface = for<'m, 'a> fn(&'m Manifest, Rows<'a>, &Observation<'a>) -> Option<&'m Rule>;

pub(super) struct Adapter {
    pub capture: CapturePolicy,
    pub evidence_names: &'static [&'static str],
    pub observation_token: fn(&Observation<'_>) -> Option<u64>,
    pub evaluate: EvaluateSurface,
    #[cfg(test)]
    pub rule_matches: fn(&Manifest, &str, Rows<'_>, &Observation<'_>) -> bool,
}

pub(super) static PLAIN: Adapter = Adapter {
    capture: CapturePolicy {
        scope: CaptureScope::History(50),
        fetch_missing_title: false,
        observe_ownership: false,
    },
    evidence_names: &[],
    observation_token: |_| None,
    evaluate: |manifest, rows, observation| {
        let screen = Screen::new(rows, observation.title, &[]);
        manifest.evaluate(&screen, observation.hook)
    },
    #[cfg(test)]
    rule_matches: |manifest, rule, rows, observation| {
        let screen = Screen::new(rows, observation.title, &[]);
        manifest.rule_matches(rule, &screen, observation.hook)
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_reuse_keeps_the_visible_surface_separate_from_scrollback() {
        for (scope, captured_lines, reusable) in [
            (CaptureScope::Visible, 0, true),
            (CaptureScope::Visible, 50, false),
            (CaptureScope::History(50), 0, false),
            (CaptureScope::History(50), 49, false),
            (CaptureScope::History(50), 50, true),
            (CaptureScope::History(50), 100, true),
        ] {
            assert_eq!(
                scope.can_reuse(captured_lines),
                reusable,
                "{scope:?}, {captured_lines}"
            );
        }
    }
}
