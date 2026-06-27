//! Plugin-adoption census for `usage_snapshot` (#2367).
//!
//! Two maps, both fed straight from the loaded [`PluginRegistry`]:
//! - `plugins_by_source`: installed count per source bucket
//!   (`builtin` / `featured` / `community` / `local`). A count by category,
//!   never an identity, so it is safe for every source.
//! - `plugins_active`: active state for the plugins whose identity is safe to
//!   name, builtin (compiled in) and featured (in the curated index). An
//!   unfeatured GitHub install (possibly a private repo) or a local-directory
//!   install is counted in `plugins_by_source` but never named here.
//!
//! The named-id allowlist rests on [`ValidationState`]: `Featured` is
//! re-derived live from the embedded index and the on-disk tree hash, so a
//! community plugin cannot reach the `Featured` arm by reusing a featured id.

use std::collections::BTreeMap;

use crate::plugin::registry::{LoadedPlugin, ValidationState};

/// Build the `(plugins_by_source, plugins_active)` census from the loaded
/// plugins. Pure over the slice, no disk reads.
pub fn census(plugins: &[LoadedPlugin]) -> (BTreeMap<String, u32>, BTreeMap<String, bool>) {
    let mut by_source: BTreeMap<String, u32> = BTreeMap::new();
    let mut active: BTreeMap<String, bool> = BTreeMap::new();
    for p in plugins {
        *by_source
            .entry(p.validation.as_str().to_string())
            .or_insert(0) += 1;
        if matches!(
            p.validation,
            ValidationState::Builtin | ValidationState::Featured
        ) {
            active.insert(p.id().to_string(), p.active());
        }
    }
    (by_source, active)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aoe_plugin_api::{PluginManifest, TrustLevel};

    fn plugin(id: &str, validation: ValidationState, enabled: bool, granted: bool) -> LoadedPlugin {
        let manifest = PluginManifest::from_toml_str(&format!(
            "id = \"{id}\"\nname = \"n\"\nversion = \"1.0.0\"\napi_version = 1\ndescription = \"d\"\n"
        ))
        .expect("valid manifest");
        let trust = match validation {
            ValidationState::Builtin => TrustLevel::Builtin,
            _ => TrustLevel::Community,
        };
        LoadedPlugin {
            manifest,
            enabled,
            trust,
            validation,
            source: None,
            dir: None,
            manifest_hash: "sha256:0".to_string(),
            granted,
        }
    }

    #[test]
    fn by_source_counts_every_source_active_names_only_safe_ids() {
        let plugins = vec![
            plugin("aoe.web", ValidationState::Builtin, true, true),
            plugin("acme.featured", ValidationState::Featured, true, true),
            plugin("acme.community", ValidationState::Community, true, true),
            plugin("acme.local", ValidationState::Local, true, true),
        ];
        let (by_source, active) = census(&plugins);

        assert_eq!(by_source.get("builtin"), Some(&1));
        assert_eq!(by_source.get("featured"), Some(&1));
        assert_eq!(by_source.get("community"), Some(&1));
        assert_eq!(by_source.get("local"), Some(&1));

        // Only builtin + featured ids are named; community / local never are.
        assert_eq!(active.get("aoe.web"), Some(&true));
        assert_eq!(active.get("acme.featured"), Some(&true));
        assert_eq!(active.get("acme.community"), None);
        assert_eq!(active.get("acme.local"), None);
    }

    #[test]
    fn active_reflects_enabled_and_granted() {
        let plugins = vec![
            plugin("on", ValidationState::Featured, true, true),
            plugin("disabled", ValidationState::Featured, false, true),
            plugin("ungranted", ValidationState::Featured, true, false),
        ];
        let (_by_source, active) = census(&plugins);
        assert_eq!(active.get("on"), Some(&true));
        assert_eq!(active.get("disabled"), Some(&false));
        assert_eq!(active.get("ungranted"), Some(&false));
    }

    #[test]
    fn community_plugin_reusing_a_featured_id_is_not_named() {
        // A community install cannot reach the Featured arm, so even an id
        // collision with a real featured plugin stays anonymous.
        let plugins = vec![plugin(
            "acme.featured",
            ValidationState::Community,
            true,
            true,
        )];
        let (by_source, active) = census(&plugins);
        assert_eq!(by_source.get("community"), Some(&1));
        assert!(active.is_empty());
    }

    #[test]
    fn same_source_twice_accumulates() {
        let plugins = vec![
            plugin("a", ValidationState::Community, true, true),
            plugin("b", ValidationState::Community, true, true),
        ];
        let (by_source, active) = census(&plugins);
        assert_eq!(by_source.get("community"), Some(&2));
        assert!(active.is_empty());
    }
}
