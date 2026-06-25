//! Plugin registry: the compiled-in first-party plugins, the externally
//! installed ones, and each plugin's enabled / granted state.
//!
//! Builtin plugins are embedded from `plugins/` in this repository and are
//! fully trusted: their capabilities are auto-granted. External plugins are
//! loaded from `<app_dir>/plugins/<id>/`; they are community-trusted, so their
//! contributions go live only once the user has granted the capability set the
//! installed manifest declares (the grant is pinned to the manifest hash).

use std::path::PathBuf;

use aoe_plugin_api::{PluginManifest, TrustLevel};

use crate::session::{CapabilityGrant, Config};

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
    // surface aoe.web; a TUI-only build has an empty builtin set.
    #[cfg(feature = "serve")]
    BuiltinPlugin {
        manifest_toml: include_str!("../../plugins/aoe-web/aoe-plugin.toml"),
    },
];

/// Whether `id` belongs to a compiled-in builtin plugin.
pub fn is_builtin_id(id: &str) -> bool {
    BUILTINS.iter().any(|b| {
        PluginManifest::from_toml_str(b.manifest_toml)
            .map(|m| m.id.as_str() == id)
            .unwrap_or(false)
    })
}

/// One loaded plugin: its manifest, trust, and enabled / granted state.
pub struct LoadedPlugin {
    pub manifest: PluginManifest,
    /// Resolved from `Config.plugins`; defaults on.
    pub enabled: bool,
    /// Builtin (auto-granted) or community (capabilities gated).
    pub trust: TrustLevel,
    /// Install source for an external plugin; `None` for builtins.
    pub source: Option<String>,
    /// On-disk directory for an external plugin; `None` for builtins.
    pub dir: Option<PathBuf>,
    /// `sha256:<hex>` of the installed manifest bytes (builtins: of the embedded
    /// TOML). A grant must be pinned to this exact hash to count.
    pub manifest_hash: String,
    /// Whether the user's grant covers the installed manifest's capability set.
    /// Always true for builtins.
    pub granted: bool,
}

impl LoadedPlugin {
    pub fn id(&self) -> &str {
        self.manifest.id.as_str()
    }

    pub fn builtin(&self) -> bool {
        matches!(self.trust, TrustLevel::Builtin)
    }

    /// Whether the plugin's contributions are live: enabled, and (for community
    /// plugins) granted against the installed manifest. An ungranted or
    /// stale-grant community plugin contributes nothing until re-approved.
    pub fn active(&self) -> bool {
        self.enabled && self.granted
    }

    /// A community plugin whose grant does not cover the installed manifest:
    /// installed but inactive, awaiting `aoe plugin update` / re-approval.
    pub fn needs_reapproval(&self) -> bool {
        !self.builtin() && !self.granted
    }
}

/// Whether a stored grant covers the installed manifest: it must be pinned to
/// the same manifest hash and include every capability the manifest declares.
fn grant_covers(grant: &CapabilityGrant, manifest: &PluginManifest, manifest_hash: &str) -> bool {
    grant.manifest_hash == manifest_hash
        && manifest
            .capabilities
            .iter()
            .all(|c| grant.capabilities.iter().any(|g| g == c.as_str()))
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
                    let manifest_hash =
                        PluginManifest::hash_bytes(builtin.manifest_toml.as_bytes());
                    plugins.push(LoadedPlugin {
                        manifest,
                        enabled,
                        trust: TrustLevel::Builtin,
                        source: None,
                        dir: None,
                        manifest_hash,
                        granted: true,
                    });
                }
                Err(e) => {
                    // A broken builtin manifest is a build defect; tested in CI.
                    load_errors.push(format!("builtin manifest invalid: {e}"));
                }
            }
        }

        load_external(config, &mut plugins, &mut load_errors);

        Self {
            plugins,
            load_errors,
        }
    }

    /// Every loaded plugin.
    pub fn all(&self) -> &[LoadedPlugin] {
        &self.plugins
    }

    /// Plugins whose contributions are live (enabled and granted).
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

/// Load external plugins from `<app_dir>/plugins/<id>/aoe-plugin.toml`. Each
/// problem is collected as a non-fatal load error rather than aborting the set.
fn load_external(config: &Config, plugins: &mut Vec<LoadedPlugin>, load_errors: &mut Vec<String>) {
    let root = match super::plugins_dir() {
        Ok(root) => root,
        Err(e) => {
            load_errors.push(format!("cannot resolve plugins dir: {e}"));
            return;
        }
    };
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        // No plugins dir yet is normal; anything else is worth surfacing.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            load_errors.push(format!("reading {}: {e}", root.display()));
            return;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                load_errors.push(format!("reading an entry in {}: {e}", root.display()));
                continue;
            }
        };
        let dir = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Skip the staging scratch dirs and other dotfiles.
        if name.starts_with('.') || !dir.is_dir() {
            continue;
        }
        let manifest_path = dir.join("aoe-plugin.toml");
        let bytes = match std::fs::read(&manifest_path) {
            Ok(bytes) => bytes,
            // A directory without a manifest is simply not a plugin; anything
            // else (a permission error, a short read) is worth surfacing.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                load_errors.push(format!("reading {}: {e}", manifest_path.display()));
                continue;
            }
        };
        let manifest = match std::str::from_utf8(&bytes)
            .map_err(|e| e.to_string())
            .and_then(|t| PluginManifest::from_toml_str(t).map_err(|e| e.to_string()))
        {
            Ok(manifest) => manifest,
            Err(e) => {
                load_errors.push(format!("plugin at {}: {e}", dir.display()));
                continue;
            }
        };
        let id = manifest.id.as_str().to_string();

        if manifest.id.is_reserved_namespace() {
            load_errors.push(format!(
                "plugin {id:?} at {} uses a reserved namespace and was skipped",
                dir.display()
            ));
            continue;
        }
        if is_builtin_id(&id) || plugins.iter().any(|p| p.id() == id) {
            load_errors.push(format!(
                "plugin {id:?} at {} collides with an existing plugin id and was skipped",
                dir.display()
            ));
            continue;
        }

        let manifest_hash = PluginManifest::hash_bytes(&bytes);
        let plugin_config = config.plugins.get(&id);
        let enabled = plugin_config.map(|p| p.enabled).unwrap_or(true);
        let source = plugin_config.and_then(|p| p.source.clone());
        let granted = plugin_config
            .and_then(|p| p.grant.as_ref())
            .map(|g| grant_covers(g, &manifest, &manifest_hash))
            .unwrap_or(false);

        plugins.push(LoadedPlugin {
            manifest,
            enabled,
            trust: TrustLevel::Community,
            source,
            dir: Some(dir),
            manifest_hash,
            granted,
        });
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

    #[test]
    fn grant_covers_requires_matching_hash_and_caps() {
        let manifest = PluginManifest::from_toml_str(
            r#"
id = "acme.thing"
name = "Thing"
version = "1.0.0"
api_version = 2
capabilities = ["net", "fs.read"]
"#,
        )
        .unwrap();
        let hash = "sha256:abc";

        let full = CapabilityGrant {
            manifest_hash: hash.to_string(),
            capabilities: vec!["net".into(), "fs.read".into()],
            granted_at: chrono::Utc::now(),
        };
        assert!(grant_covers(&full, &manifest, hash));

        // Wrong hash (manifest changed since the grant): not covered.
        let stale = CapabilityGrant {
            manifest_hash: "sha256:old".to_string(),
            ..full.clone()
        };
        assert!(!grant_covers(&stale, &manifest, hash));

        // Missing a capability: not covered.
        let partial = CapabilityGrant {
            manifest_hash: hash.to_string(),
            capabilities: vec!["net".into()],
            granted_at: chrono::Utc::now(),
        };
        assert!(!grant_covers(&partial, &manifest, hash));
    }
}
