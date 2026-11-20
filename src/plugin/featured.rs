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

    /// The shipped index is a maintainer attestation, so the ids and the sources they
    /// vouch for are asserted here: a copy-paste cannot silently vouch for the wrong repo.
    #[test]
    fn embedded_index_lists_the_expected_ids_and_sources() {
        let index = FeaturedIndex::from_toml_str(EMBEDDED).expect("embedded index parses");
        let expected = [
            (
                "agent-of-empires.github",
                "gh:agent-of-empires/plugin-github",
            ),
            ("agent-of-empires.cron", "gh:agent-of-empires/plugin-cron"),
            (
                "agent-of-empires.kanban",
                "gh:agent-of-empires/plugin-kanban",
            ),
            (
                "agent-of-empires.linear",
                "gh:agent-of-empires/plugin-linear",
            ),
            (
                "agent-of-empires.aoe-freebuf",
                "gh:agent-of-empires/plugin-freebuf",
            ),
            ("agent-of-empires.omp", "gh:agent-of-empires/plugin-omp"),
        ];
        for (id, source) in expected {
            let entry = index.get(id).unwrap_or_else(|| panic!("{id} is listed"));
            assert_eq!(entry.source, source, "{id} names its own repository");
            assert!(!entry.versions.is_empty(), "{id} has no vetted release");
            assert!(
                entry.versions.values().all(|v| v.starts_with("sha256:")),
                "{id} has a non-sha256 version value"
            );
        }
    }
}
