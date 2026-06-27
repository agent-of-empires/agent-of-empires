//! The curated / featured plugin index.
//!
//! `plugins/featured.toml` is compiled into the binary and pins a vetted plugin
//! release to its source [`tree_hash`](super::integrity::tree_hash). A featured
//! entry is the maintainer's attestation that this exact tree was reviewed: it
//! is what makes "is this plugin safe" answerable, and it is the only thing
//! that lets a community install claim a reserved (`aoe.*` /
//! `agent-of-empires.*`) namespace. Install and update refuse on a mismatch
//! against the pin.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Deserialize;

/// The compiled-in index. Ships effectively empty; entries land as maintainers
/// vet plugin releases.
const EMBEDDED: &str = include_str!("../../plugins/featured.toml");

/// One vetted pin, keyed by plugin id in the index.
#[derive(Debug, Clone, Deserialize)]
pub struct FeaturedEntry {
    /// The canonical source slug the plugin must be installed from
    /// (`gh:owner/repo`).
    pub source: String,
    /// `sha256:<hex>` of the vetted source tree.
    pub tree_hash: String,
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
tree_hash = "sha256:abc"
"#,
        )
        .unwrap();
        let entry = index.get("agent-of-empires.example").expect("present");
        assert_eq!(entry.source, "gh:agent-of-empires/example");
        assert_eq!(entry.tree_hash, "sha256:abc");
        assert!(index.get("acme.absent").is_none());

        // Source-slug match is case-insensitive and ignores the keying id.
        assert!(index.is_featured_source("gh:agent-of-empires/example"));
        assert!(index.is_featured_source("gh:Agent-Of-Empires/Example"));
        assert!(!index.is_featured_source("gh:someone/else"));
    }
}
