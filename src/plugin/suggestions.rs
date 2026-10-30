//! Host-side checks for the suggestions worker RPCs.
//!
//! Nothing is stored yet: `suggestions.set` validates and acknowledges, and a
//! source is enabled iff its manifest `default_enabled` says so.
use std::collections::{BTreeSet, HashSet};

use aoe_plugin_api::suggestions::MAX_KEY_BYTES;
use aoe_plugin_api::{SetSuggestions, Suggestion, SuggestionsError, SuggestionsSpec};
use serde_json::{json, Value};

use super::host_api::DispatchError;

const MAX_ITEMS: usize = 500;
const MAX_TITLE_BYTES: usize = 512;
const MAX_REASON_BYTES: usize = 2048;
const MAX_URL_BYTES: usize = 2048;
const MAX_PROJECT_BYTES: usize = 4096;
const MAX_PREVIEW_BYTES: usize = 16 * 1024;
const MAX_PROMPT_BYTES: usize = 64 * 1024;
const MAX_VARIABLE_VALUE_BYTES: usize = 2048;
const MAX_ERROR_BYTES: usize = 4096;

pub(super) fn sources(spec: &SuggestionsSpec) -> Value {
    let sources: Vec<Value> = spec
        .sources
        .iter()
        .map(|s| json!({ "criterion": s.criterion, "enabled": s.default_enabled }))
        .collect();
    json!({ "sources": sources })
}

pub(super) fn set(spec: &SuggestionsSpec, params: &Value) -> Result<Value, DispatchError> {
    let request: SetSuggestions = serde_json::from_value(params.clone()).map_err(|e| {
        DispatchError::invalid_params(format!("invalid suggestions.set params: {e}"))
    })?;
    let total = request.items.len();
    let accepted = validate_items(spec, request.items).map_err(DispatchError::invalid_params)?;
    Ok(json!({ "ok": true, "accepted": accepted.len(), "dropped": total - accepted.len() }))
}

pub(super) fn error(params: &Value) -> Result<Value, DispatchError> {
    let request: SuggestionsError = serde_json::from_value(params.clone()).map_err(|e| {
        DispatchError::invalid_params(format!("invalid suggestions.error params: {e}"))
    })?;
    if request.message.len() > MAX_ERROR_BYTES {
        return Err(DispatchError::invalid_params(format!(
            "suggestions.error message is limited to {MAX_ERROR_BYTES} bytes"
        )));
    }
    Ok(json!({ "ok": true }))
}

/// Returns the items of enabled criteria, or the first contract violation.
fn validate_items(
    spec: &SuggestionsSpec,
    items: Vec<Suggestion>,
) -> Result<Vec<Suggestion>, String> {
    if items.len() > MAX_ITEMS {
        return Err(format!("suggestions.set accepts at most {MAX_ITEMS} items"));
    }
    let mut ids = HashSet::new();
    for item in &items {
        validate_item(spec, item)?;
        if !ids.insert(item.id.as_str()) {
            return Err(format!("duplicate suggestion id {:?}", item.id));
        }
    }
    Ok(items
        .into_iter()
        .filter(|item| {
            spec.sources
                .iter()
                .any(|s| s.default_enabled && s.criterion == item.criterion)
        })
        .collect())
}

fn validate_item(spec: &SuggestionsSpec, item: &Suggestion) -> Result<(), String> {
    let id = &item.id;
    let source = spec
        .sources
        .iter()
        .find(|s| s.criterion == item.criterion)
        .ok_or_else(|| format!("undeclared suggestion criterion {:?}", item.criterion))?;
    for (name, value) in [
        ("id", id),
        ("title", &item.title),
        ("source_ref", &item.source_ref),
    ] {
        if value.trim().is_empty() {
            return Err(format!("suggestion {id:?}: {name} must not be empty"));
        }
    }
    let bounded = [
        ("id", id.len(), MAX_KEY_BYTES),
        ("title", item.title.len(), MAX_TITLE_BYTES),
        ("reason", item.reason.len(), MAX_REASON_BYTES),
        ("source_url", item.source_url.len(), MAX_URL_BYTES),
        ("source_ref", item.source_ref.len(), MAX_URL_BYTES),
        ("preview", item.preview.len(), MAX_PREVIEW_BYTES),
        ("draft.title", item.draft.title.len(), MAX_TITLE_BYTES),
        ("draft.prompt", item.draft.prompt.len(), MAX_PROMPT_BYTES),
        (
            "project",
            item.project.as_ref().map_or(0, String::len),
            MAX_PROJECT_BYTES,
        ),
    ];
    if let Some((name, _, max)) = bounded.iter().find(|(_, len, max)| len > max) {
        return Err(format!(
            "suggestion {id:?}: {name} is limited to {max} bytes"
        ));
    }
    if !is_http_url(&item.source_url) {
        return Err(format!(
            "suggestion {id:?}: source_url must be an http(s) URL"
        ));
    }
    let declared: BTreeSet<&str> = source.variables.iter().map(String::as_str).collect();
    let given: BTreeSet<&str> = item.variables.keys().map(String::as_str).collect();
    if declared != given {
        return Err(format!(
            "suggestion {id:?}: variables must be exactly the ones source {:?} declares",
            source.criterion
        ));
    }
    if item
        .variables
        .values()
        .any(|v| v.len() > MAX_VARIABLE_VALUE_BYTES)
    {
        return Err(format!(
            "suggestion {id:?}: variable values are limited to {MAX_VARIABLE_VALUE_BYTES} bytes"
        ));
    }
    Ok(())
}

fn is_http_url(url: &str) -> bool {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    rest.is_some_and(|r| !r.is_empty() && !r.chars().any(|c| c.is_whitespace() || c.is_control()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aoe_plugin_api::{SuggestionDraft, SuggestionSource};

    fn spec() -> SuggestionsSpec {
        let source = |criterion: &str, variables: &[&str], default_enabled| SuggestionSource {
            criterion: criterion.into(),
            label: criterion.into(),
            description: String::new(),
            variables: variables.iter().map(|v| v.to_string()).collect(),
            default_enabled,
        };
        SuggestionsSpec {
            sources: vec![
                source("assigned", &["repo"], true),
                source("quiet", &[], false),
            ],
        }
    }

    fn item(id: &str, criterion: &str) -> Suggestion {
        Suggestion {
            id: id.into(),
            title: "Work".into(),
            reason: "Assigned".into(),
            source_url: "https://example.com/1".into(),
            source_ref: "example/1".into(),
            preview: "body".into(),
            project: None,
            criterion: criterion.into(),
            variables: if criterion == "assigned" {
                [("repo".to_string(), "acme/widgets".to_string())].into()
            } else {
                Default::default()
            },
            draft: SuggestionDraft {
                title: "Work".into(),
                prompt: "Do work".into(),
            },
        }
    }

    #[test]
    fn disabled_criteria_are_dropped_and_enabled_ones_kept() {
        let kept =
            validate_items(&spec(), vec![item("a", "assigned"), item("q", "quiet")]).unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id, "a");
    }

    #[test]
    fn contract_violations_are_rejected() {
        type Mutate = fn(&mut Suggestion);
        let cases: [(&str, Mutate); 9] = [
            ("undeclared", |i| i.criterion = "nope".into()),
            ("must not be empty", |i| i.title = " ".into()),
            ("http(s)", |i| i.source_url = "javascript:alert(1)".into()),
            ("http(s)", |i| i.source_url = "https://".into()),
            ("limited to 256", |i| i.id = "x".repeat(257)),
            ("limited to 16384", |i| {
                i.preview = "x".repeat(16 * 1024 + 1)
            }),
            ("variables must be exactly", |i| i.variables.clear()),
            ("variables must be exactly", |i| {
                i.variables.insert("extra".into(), "v".into());
            }),
            ("variable values", |i| {
                i.variables.insert("repo".into(), "x".repeat(2049));
            }),
        ];
        for (expected, mutate) in cases {
            let mut bad = item("a", "assigned");
            mutate(&mut bad);
            let err = validate_items(&spec(), vec![bad]).unwrap_err();
            assert!(err.contains(expected), "{expected:?} not in {err:?}");
        }
        let dup = validate_items(&spec(), vec![item("a", "assigned"), item("a", "assigned")]);
        assert!(dup.unwrap_err().contains("duplicate"));
        let many = (0..=MAX_ITEMS)
            .map(|n| item(&n.to_string(), "assigned"))
            .collect();
        assert!(validate_items(&spec(), many)
            .unwrap_err()
            .contains("at most 500"));
    }

    #[test]
    fn set_reports_counts_and_sources_report_defaults() {
        let params = json!({ "items": [item("a", "assigned"), item("q", "quiet")] });
        assert_eq!(
            set(&spec(), &params).unwrap(),
            json!({ "ok": true, "accepted": 1, "dropped": 1 })
        );
        assert!(set(&spec(), &json!({ "items": [], "provider_id": "x" })).is_err());
        assert_eq!(
            sources(&spec()),
            json!({ "sources": [
                { "criterion": "assigned", "enabled": true },
                { "criterion": "quiet", "enabled": false },
            ]})
        );
    }

    #[test]
    fn error_message_is_bounded() {
        assert!(error(&json!({ "message": "boom" })).is_ok());
        assert!(error(&json!({ "message": "x".repeat(MAX_ERROR_BYTES + 1) })).is_err());
    }
}
