//! Plugin registry: the compiled-in first-party plugins and their
//! enabled/disabled state.
//!
//! Builtin plugins are embedded from `plugins/` in this repository. Disabling
//! one removes it from the active set on the next reload, with no residue
//! (acceptance criterion 3 of #268). External (installed) plugins return in a
//! follow-up PR.

use aoe_plugin_api::PluginManifest;

use crate::session::Config;

/// A plugin compiled into the aoe binary.
pub struct BuiltinPlugin {
    pub manifest_toml: &'static str,
}

/// First-party plugins bundled with the binary. Deliberately minimal while the
/// system is proven out: just the `aoe.web` dashboard marker (under `serve`).
/// More land as each piece is verified.
pub static BUILTINS: &[BuiltinPlugin] = &[
    // The web dashboard's management marker is present whenever the dashboard
    // is compiled in (`feature = "serve"`), so serve and release builds always
    // surface aoe.web; a TUI-only build has an empty registry.
    #[cfg(feature = "serve")]
    BuiltinPlugin {
        manifest_toml: include_str!("../../plugins/aoe-web/aoe-plugin.toml"),
    },
];

/// One loaded plugin: its manifest and whether it is enabled.
pub struct LoadedPlugin {
    pub manifest: PluginManifest,
    /// Resolved from `Config.plugins`; builtins default on.
    pub enabled: bool,
}

impl LoadedPlugin {
    pub fn id(&self) -> &str {
        self.manifest.id.as_str()
    }

    /// Whether the plugin's contributions are live. Builtins are first-party
    /// and always granted, so this is just `enabled`.
    pub fn active(&self) -> bool {
        self.enabled
    }
}

/// The set of plugins loaded for a config, plus any load problems.
pub struct PluginRegistry {
    plugins: Vec<LoadedPlugin>,
    load_errors: Vec<String>,
}

impl PluginRegistry {
    pub fn load(config: &Config) -> Self {
        let mut plugins = Vec::new();
        let mut load_errors = Vec::new();
        for builtin in BUILTINS {
            match PluginManifest::from_toml_str(builtin.manifest_toml) {
                Ok(manifest) => {
                    let enabled = config
                        .plugins
                        .get(manifest.id.as_str())
                        .map(|p| p.enabled)
                        .unwrap_or(true);
                    plugins.push(LoadedPlugin { manifest, enabled });
                }
                Err(e) => {
                    // A broken builtin manifest is a build defect; tested in CI.
                    load_errors.push(format!("builtin manifest invalid: {e}"));
                }
            }
        }
        Self {
            plugins,
            load_errors,
        }
    }

    /// Every loaded plugin.
    pub fn all(&self) -> &[LoadedPlugin] {
        &self.plugins
    }

    /// Plugins whose contributions are live (enabled).
    pub fn active(&self) -> impl Iterator<Item = &LoadedPlugin> {
        self.plugins.iter().filter(|p| p.active())
    }

    pub fn get(&self, plugin_id: &str) -> Option<&LoadedPlugin> {
        self.plugins.iter().find(|p| p.id() == plugin_id)
    }

    pub fn load_errors(&self) -> &[String] {
        &self.load_errors
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_manifests_parse_and_have_unique_ids() {
        let mut seen = std::collections::HashSet::new();
        for builtin in BUILTINS {
            let manifest = PluginManifest::from_toml_str(builtin.manifest_toml)
                .expect("builtin manifest must be valid");
            assert!(
                seen.insert(manifest.id.as_str().to_string()),
                "duplicate builtin id {}",
                manifest.id
            );
        }
    }
}
