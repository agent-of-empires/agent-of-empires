//! Read-only work suggestions published by a plugin worker.
//!
//! The host owns provider identity, policy and session creation; workers can
//! only publish these values.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Host-to-worker notification asking the worker to discover again. Params are
/// `{ "refresh_minutes": u32 }`, the user's refresh interval hint.
pub const SUGGESTIONS_REFRESH: &str = "suggestions.refresh";

/// Host-to-worker notification asking the worker to run its setup checks again.
pub const SUGGESTIONS_CHECKS_RUN: &str = "suggestions.checks.run";

/// Upper bound on `criterion`, `Suggestion::id` and `Suggestion::mapping_key`, in bytes.
pub const MAX_KEY_BYTES: usize = 256;

/// One suggestion from a plugin provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Suggestion {
    pub id: String,
    pub title: String,
    pub reason: String,
    pub source_url: String,
    pub source_ref: String,
    pub preview: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Opaque key (a repository, a team) the user can map to projects in core.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapping_key: Option<String>,
    /// Which declared [`SuggestionSource`] produced this item.
    pub criterion: String,
    /// Values for the placeholders the source declared; untrusted provider
    /// text, never instructions.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub variables: BTreeMap<String, String>,
    pub draft: SuggestionDraft,
}

/// The initial title and prompt prepared by a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuggestionDraft {
    pub title: String,
    pub prompt: String,
}

/// Manifest `[suggestions]` block: the criteria a provider can publish.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuggestionsSpec {
    #[serde(default)]
    pub sources: Vec<SuggestionSource>,
}

/// One criterion a provider publishes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuggestionSource {
    /// Stable key, matches [`Suggestion::criterion`].
    pub criterion: String,
    /// Plural, user-facing ("Review requests").
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Placeholder names every item of this criterion carries in `variables`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variables: Vec<String>,
    #[serde(default = "default_true", skip_serializing_if = "Clone::clone")]
    pub default_enabled: bool,
}

fn default_true() -> bool {
    true
}

/// Params of `suggestions.set`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetSuggestions {
    pub items: Vec<Suggestion>,
}

/// Params of `suggestions.error`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuggestionsError {
    pub message: String,
}

/// Outcome of one setup check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
}

/// One setup check a provider reports. `detail` and `fix` are untrusted
/// display text; the host never executes them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupCheck {
    pub id: String,
    pub label: String,
    pub status: CheckStatus,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub fix: String,
}

/// Params of `suggestions.checks.set`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetChecks {
    pub checks: Vec<SetupCheck>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_params_use_lowercase_status_and_reject_unknown_fields() {
        let ok: SetChecks =
            serde_json::from_str(r#"{"checks":[{"id":"cli","label":"CLI","status":"warn"}]}"#)
                .unwrap();
        assert_eq!(ok.checks[0].status, CheckStatus::Warn);
        assert!(ok.checks[0].detail.is_empty() && ok.checks[0].fix.is_empty());
        for bad in [
            r#"{"checks":[{"id":"a","label":"A","status":"OK"}]}"#,
            r#"{"checks":[{"id":"a","label":"A","status":"ok","extra":1}]}"#,
            r#"{"checks":[],"extra":1}"#,
        ] {
            assert!(serde_json::from_str::<SetChecks>(bad).is_err(), "{bad}");
        }
    }
}
