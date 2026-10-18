//! Read-only work suggestions published by a plugin worker.
//!
//! The host owns provider identity, persistence, policy, and session creation;
//! workers can only replace their own snapshot with these values.
use serde::{Deserialize, Serialize};

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
    pub criterion: String,
    pub draft: SuggestionDraft,
}

/// The user-visible initial title and prompt prepared by a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuggestionDraft {
    pub title: String,
    pub prompt: String,
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
