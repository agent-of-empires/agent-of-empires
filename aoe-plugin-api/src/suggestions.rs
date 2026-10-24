//! Read-only work suggestions published by a plugin worker.
//!
//! The host owns provider identity, persistence, policy, and session creation;
//! workers can only replace their own snapshot with these values.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
    /// Values for the `{name}` placeholders the source declared; untrusted
    /// provider text, never instructions.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub variables: BTreeMap<String, String>,
    pub draft: SuggestionDraft,
}

/// The user-visible initial title and prompt prepared by a provider.
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

/// One criterion a provider publishes. The host shows `label` as a per-profile
/// switch; a switched-off criterion is reported to the worker through
/// `suggestions.sources` so it can skip the query, and its items are dropped
/// on publish regardless.
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

/// Request body for `suggestions.set { items }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetSuggestions {
    pub items: Vec<Suggestion>,
}

/// Request body for `suggestions.error { message }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuggestionsError {
    pub message: String,
}
