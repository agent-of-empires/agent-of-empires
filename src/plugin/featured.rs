//! The curated / featured plugin index.
//!
//! `plugins/featured.toml` is compiled into the binary and pins one or more
//! vetted plugin releases to their source
//! [`tree_hash`](super::integrity::tree_hash). A featured entry is the
//! maintainer's attestation that each listed tree was reviewed: it is what makes
//! "is this plugin safe" answerable, and it is the only thing that lets a
//! community install claim a reserved (`aoe.*` / `agent-of-empires.*`)
//! namespace. An entry holds a `version -> tree_hash` map so a new release can
//! be vetted alongside older ones; an install whose fetched tree hashes to any
//! vetted value is featured-verified.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Deserialize;

/// The compiled-in index. Ships effectively empty; entries land as maintainers
/// vet plugin releases.
const EMBEDDED: &str = include_str!("../../plugins/featured.toml");

/// One featured plugin's vetted releases, keyed by plugin id in the index.
#[derive(Debug, Clone, Deserialize)]
pub struct FeaturedEntry {
    /// The canonical source slug the plugin must be installed from
    /// (`gh:owner/repo`).
    pub source: String,
    /// Vetted releases as `version -> sha256:<hex>` of the source tree. The
    /// version label is informational (it documents which release each hash
    /// belongs to); the verified decision is membership in the set of values.
    pub versions: BTreeMap<String, String>,
}

impl FeaturedEntry {
    /// Whether `tree_hash` is one of this entry's vetted release hashes.
    pub fn verifies(&self, tree_hash: &str) -> bool {
        self.versions.values().any(|v| v == tree_hash)
    }
}

/// The parsed featured index.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FeaturedIndex {
    #[serde(default)]
    plugins: BTreeMap<String, FeaturedEntry>,
}

impl FeaturedIndex {
    /// Load the curated index.
    ///
    /// In debug builds `AOE_FEATURED_INDEX_PATH` overrides the embedded file so
    /// tests can supply their own pins. Release builds ALWAYS use the
    /// compiled-in index: the curated set is a root of trust, so it must not be
    /// redefinable by the process environment in a shipped binary (an env
    /// override would let any caller elevate a malicious plugin into a reserved
    /// namespace).
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

    /// Whether any featured entry is pinned to this source slug (case-insensitive,
    /// GitHub slugs are not case-sensitive). Discovery uses this to badge a search
    /// result as a featured *source*, without fetching its manifest; it is not a
    /// claim that the repo's current tree matches the pinned `tree_hash`, which
    /// only install-time `verify_featured` enforces.
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
    fn embedded_index_parses() {
        // A broken embedded featured.toml is a build defect; catch it in CI.
        FeaturedIndex::from_toml_str(EMBEDDED).expect("embedded featured.toml must parse");
    }

    #[test]
    fn looks_up_by_id() {
        let index = FeaturedIndex::from_toml_str(
            r#"
[plugins."agent-of-empires.example"]
source = "gh:agent-of-empires/example"
versions = { "1.0" = "sha256:abc" }
"#,
        )
        .unwrap();
        let entry = index.get("agent-of-empires.example").expect("present");
        assert_eq!(entry.source, "gh:agent-of-empires/example");
        assert_eq!(
            entry.versions.get("1.0").map(String::as_str),
            Some("sha256:abc")
        );
        assert!(index.get("acme.absent").is_none());

        // Source-slug match is case-insensitive and ignores the keying id.
        assert!(index.is_featured_source("gh:agent-of-empires/example"));
        assert!(index.is_featured_source("gh:Agent-Of-Empires/Example"));
        assert!(!index.is_featured_source("gh:someone/else"));
    }

    #[test]
    fn verifies_any_vetted_hash() {
        let index = FeaturedIndex::from_toml_str(
            r#"
[plugins."agent-of-empires.example"]
source = "gh:agent-of-empires/example"
versions = { "1.0" = "sha256:aaa", "1.1" = "sha256:bbb" }
"#,
        )
        .unwrap();
        let entry = index.get("agent-of-empires.example").expect("present");
        assert!(entry.verifies("sha256:aaa"));
        assert!(entry.verifies("sha256:bbb"));
        assert!(!entry.verifies("sha256:ccc"));
    }
}
