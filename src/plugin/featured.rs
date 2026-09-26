//! The curated / featured plugin index.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Deserialize;

const EMBEDDED: &str = include_str!("../../plugins/featured.toml");

#[derive(Debug, Clone, Deserialize)]
pub struct FeaturedEntry {
    pub source: String,
    pub versions: BTreeMap<String, String>,
}

impl FeaturedEntry {
    pub fn verifies(&self, tree_hash: &str) -> bool {
        self.versions.values().any(|v| v == tree_hash)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FeaturedIndex {
    #[serde(default)]
    plugins: BTreeMap<String, FeaturedEntry>,
}

impl FeaturedIndex {
    pub fn load() -> Result<Self> {
        #[cfg(debug_assertions)]
        if let Ok(path) = std::env::var("AOE_FEATURED_INDEX_PATH") {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading AOE_FEATURED_INDEX_PATH {path}"))?;
            return Self::from_toml_str(&text);
        }
        Self::from_toml_str(EMBEDDED)
    }

    pub fn from_toml_str(text: &str) -> Result<Self> {
        toml::from_str(text).context("parsing featured plugin index")
    }

    pub fn get(&self, id: &str) -> Option<&FeaturedEntry> {
        self.plugins.get(id)
    }

    pub fn is_featured_source(&self, slug: &str) -> bool {
        self.plugins
            .values()
            .any(|e| e.source.eq_ignore_ascii_case(slug))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_up_by_id_and_verifies_any_vetted_hash() {
        FeaturedIndex::from_toml_str(EMBEDDED).expect("embedded featured.toml must parse");
        let index = FeaturedIndex::from_toml_str(
            r#"
[plugins."agent-of-empires.example"]
source = "gh:agent-of-empires/example"
versions = { "1.0" = "sha256:aaa", "1.1" = "sha256:bbb" }
"#,
        )
        .unwrap();

        let entry = index.get("agent-of-empires.example").expect("present");
        assert_eq!(entry.source, "gh:agent-of-empires/example");
        assert_eq!(
            entry.versions.get("1.0").map(String::as_str),
            Some("sha256:aaa")
        );
        assert!(index.get("acme.absent").is_none());

        assert!(index.is_featured_source("gh:agent-of-empires/example"));
        assert!(
            index.is_featured_source("gh:Agent-Of-Empires/Example"),
            "the source match is case-insensitive"
        );
        assert!(!index.is_featured_source("gh:someone/else"));

        assert!(entry.verifies("sha256:aaa"));
        assert!(entry.verifies("sha256:bbb"));
        assert!(!entry.verifies("sha256:ccc"));
    }
}
