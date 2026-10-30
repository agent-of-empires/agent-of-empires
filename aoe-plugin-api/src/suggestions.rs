//! Read-only work suggestions published by a plugin worker.
//!
//! The host owns provider identity, policy and session creation; workers can
//! only publish these values.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Host-to-worker notification asking the worker to discover again.
pub const SUGGESTIONS_REFRESH: &str = "suggestions.refresh";

/// Upper bound on `criterion` and `Suggestion::id`, in bytes.
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
