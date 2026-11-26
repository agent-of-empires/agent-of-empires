//! Suggested-sessions telemetry: adoption state, volume buckets and per-kind start counts.
//! Funnel counts live in the [`feature_use`](super::feature_use) registry. Never reported:
//! titles, reasons, references, repo names, URLs, prompts, preset contents, project paths.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde_json::Value;

use crate::plugin::registry::ValidationState;

/// Distinct `provider/kind` keys held between snapshots; the rest count as `other`.
const MAX_KINDS: usize = 32;

static KINDS: Mutex<BTreeMap<String, u32>> = Mutex::new(BTreeMap::new());

fn kinds() -> std::sync::MutexGuard<'static, BTreeMap<String, u32>> {
    KINDS.lock().unwrap_or_else(|e| e.into_inner())
}

/// `provider/kind` for a builtin or featured plugin, else `other`: a community plugin's ids
/// and kinds are its own text and never leave the machine.
fn kind_key(provider_id: &str, criterion: &str) -> String {
    let known = crate::plugin::registry().get(provider_id).is_some_and(|p| {
        matches!(
            p.validation,
            ValidationState::Builtin | ValidationState::Featured
        )
    });
    if known {
        format!("{provider_id}/{criterion}")
    } else {
        "other".to_string()
    }
}

/// Counts one Session started from a suggestion of this kind.
pub fn record_kind(provider_id: &str, criterion: &str) {
    let key = kind_key(provider_id, criterion);
    super::consent::record(|| {
        let mut kinds = kinds();
        let key = if kinds.contains_key(&key) || kinds.len() < MAX_KINDS {
            key
        } else {
            "other".to_string()
        };
        *kinds.entry(key).or_insert(0) += 1;
    });
}

pub fn kinds_snapshot() -> BTreeMap<String, u32> {
    kinds().clone()
}

/// Called only after a confirmed send, so starts recorded mid-send survive.
pub fn clear_reported_kinds(reported: &BTreeMap<String, u32>) {
    let mut kinds = kinds();
    for (key, amount) in reported {
        if let Some(count) = kinds.get_mut(key) {
            *count = count.saturating_sub(*amount);
            if *count == 0 {
                kinds.remove(key);
            }
        }
    }
}

pub(super) fn zero() {
    kinds().clear();
}

pub fn bucket_sources(count: usize) -> &'static str {
    match count {
        0 => "0",
        1 => "1",
        2..=3 => "2-3",
        _ => "4+",
    }
}

pub fn bucket_open(count: usize) -> &'static str {
    match count {
        0 => "0",
        1..=5 => "1-5",
        6..=20 => "6-20",
        21..=100 => "21-100",
        _ => "100+",
    }
}

/// Adoption state read from a `GET /api/suggestions` body: three flags and two buckets.
pub struct Adoption {
    pub features: BTreeMap<String, bool>,
    pub prefs: BTreeMap<String, String>,
}

pub fn adoption(snapshot: &Value) -> Adoption {
    let enabled_sources: usize = snapshot["providers"]
        .as_array()
        .map(|providers| {
            providers
                .iter()
                .filter(|p| p["enabled"].as_bool().unwrap_or(false))
                .map(|p| {
                    p["sources"].as_array().map_or(0, |sources| {
                        sources
                            .iter()
                            .filter(|s| s["enabled"].as_bool().unwrap_or(false))
                            .count()
                    })
                })
                .sum()
        })
        .unwrap_or(0);
    let open = snapshot["items"].as_array().map_or(0, |items| {
        items
            .iter()
            .filter(|i| !i["dismissed"].as_bool().unwrap_or(false))
            .count()
    });
    let prefs = &snapshot["prefs"];
    Adoption {
        features: BTreeMap::from([
            ("suggestions_on".to_string(), enabled_sources > 0),
            (
                "suggestions_homepage".to_string(),
                prefs["homepage"].as_bool().unwrap_or(false),
            ),
            (
                "suggestions_sidebar".to_string(),
                prefs["sidebar"].as_bool().unwrap_or(false),
            ),
        ]),
        prefs: BTreeMap::from([
            (
                "suggestions_sources".to_string(),
                bucket_sources(enabled_sources).to_string(),
            ),
            (
                "suggestions_open".to_string(),
                bucket_open(open).to_string(),
            ),
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn buckets_cover_their_edges() {
        for (n, sources, open) in [
            (0, "0", "0"),
            (1, "1", "1-5"),
            (3, "2-3", "1-5"),
            (5, "4+", "1-5"),
            (6, "4+", "6-20"),
            (20, "4+", "6-20"),
            (21, "4+", "21-100"),
            (100, "4+", "21-100"),
            (101, "4+", "100+"),
        ] {
            assert_eq!(bucket_sources(n), sources, "{n}");
            assert_eq!(bucket_open(n), open, "{n}");
        }
    }

    #[test]
    fn adoption_reads_only_flags_and_counts() {
        let snapshot = json!({
            "providers": [
                { "enabled": true, "sources": [{ "enabled": true }, { "enabled": false }, { "enabled": true }] },
                { "enabled": false, "sources": [{ "enabled": true }] },
            ],
            "items": [
                { "dismissed": false, "title": "secret title" },
                { "dismissed": true },
                { "dismissed": false },
            ],
            "prefs": { "homepage": true, "sidebar": false },
        });
        let adoption = adoption(&snapshot);
        assert_eq!(adoption.features["suggestions_on"], true);
        assert_eq!(adoption.features["suggestions_homepage"], true);
        assert_eq!(adoption.features["suggestions_sidebar"], false);
        assert_eq!(adoption.prefs["suggestions_sources"], "2-3");
        assert_eq!(adoption.prefs["suggestions_open"], "1-5");
        let none = super::adoption(&json!({ "providers": [], "items": [] }));
        assert_eq!(none.features["suggestions_on"], false);
        assert_eq!(none.prefs["suggestions_open"], "0");
    }

    #[test]
    fn kind_counts_drop_exactly_what_was_reported_and_unknown_plugins_are_other() {
        assert_eq!(kind_key("no.such.plugin", "assigned"), "other");
        {
            let mut held = kinds();
            held.clear();
            held.insert("a/b".to_string(), 3);
            held.insert("other".to_string(), 1);
        }
        let reported = kinds_snapshot();
        kinds().insert("a/b".to_string(), 4);
        clear_reported_kinds(&reported);
        assert_eq!(kinds_snapshot(), BTreeMap::from([("a/b".to_string(), 1)]));
        zero();
        assert!(kinds_snapshot().is_empty());
    }
}
