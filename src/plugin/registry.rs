//! Plugin registry: the compiled-in first-party plugins, the externally

use std::path::{Path, PathBuf};

use aoe_plugin_api::{PluginManifest, TrustLevel};

use super::featured::FeaturedIndex;
use super::integrity;
use crate::session::{CapabilityGrant, Config};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationState {
    Builtin,
    Featured,
    Community,
    Local,
}

impl ValidationState {
    pub fn as_str(self) -> &'static str {
        match self {
            ValidationState::Builtin => "builtin",
            ValidationState::Featured => "featured",
            ValidationState::Community => "community",
            ValidationState::Local => "local",
        }
    }
}

pub struct BuiltinPlugin {
    pub manifest_toml: &'static str,
}

pub static BUILTINS: &[BuiltinPlugin] = &[
    #[cfg(feature = "web")]
    BuiltinPlugin {
        manifest_toml: include_str!("../../plugins/aoe-web/aoe-plugin.toml"),
    },
];

pub fn is_builtin_id(id: &str) -> bool {
    BUILTINS.iter().any(|b| {
        PluginManifest::from_toml_str(b.manifest_toml)
            .map(|m| m.id.as_str() == id)
            .unwrap_or(false)
    })
}

pub struct LoadedPlugin {
    pub manifest: PluginManifest,
    pub enabled: bool,
    pub trust: TrustLevel,
    pub validation: ValidationState,
    pub source: Option<String>,
    pub dir: Option<PathBuf>,
    pub manifest_hash: String,
    pub granted: bool,
}

impl LoadedPlugin {
    pub fn id(&self) -> &str {
        self.manifest.id.as_str()
    }

    pub fn builtin(&self) -> bool {
        matches!(self.trust, TrustLevel::Builtin)
    }

    pub fn active(&self) -> bool {
        self.enabled && self.granted
    }

    pub fn needs_reapproval(&self) -> bool {
        !self.builtin() && !self.granted
    }
}

fn grant_covers(grant: &CapabilityGrant, manifest: &PluginManifest, manifest_hash: &str) -> bool {
    grant.manifest_hash == manifest_hash
        && manifest
            .capabilities
            .iter()
            .all(|c| grant.capabilities.iter().any(|g| g == c.as_str()))
}

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
                        validation: ValidationState::Builtin,
                        source: None,
                        dir: None,
                        manifest_hash,
                        granted: true,
                    });
                }
                Err(e) => {
                    load_errors.push(format!("builtin manifest invalid: {e}"));
                }
            }
        }

        let featured = FeaturedIndex::load().unwrap_or_else(|e| {
            load_errors.push(format!("reading featured plugin index: {e:#}"));
            FeaturedIndex::default()
        });
        load_external(config, &featured, &mut plugins, &mut load_errors);

        Self {
            plugins,
            load_errors,
        }
    }

    pub fn all(&self) -> &[LoadedPlugin] {
        &self.plugins
    }

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

/// `Featured` is re-derived from the embedded index and tree hash, never trusted from the
/// user-writable lockfile, because it also lifts the reserved-namespace gate.
fn validation_for(
    featured: &FeaturedIndex,
    id: &str,
    dir: &Path,
    source: Option<&str>,
) -> ValidationState {
    if let Some(entry) = featured.get(id) {
        if integrity::tree_hash(dir).is_ok_and(|h| entry.verifies(&h)) {
            return ValidationState::Featured;
        }
    }
    match source {
        Some(s) if s.starts_with("gh:") => ValidationState::Community,
        _ => ValidationState::Local,
    }
}

fn load_external(
    config: &Config,
    featured: &FeaturedIndex,
    plugins: &mut Vec<LoadedPlugin>,
    load_errors: &mut Vec<String>,
) {
    let root = match super::plugins_dir() {
        Ok(root) => root,
        Err(e) => {
            load_errors.push(format!("cannot resolve plugins dir: {e}"));
            return;
        }
    };
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
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
        if name.starts_with('.') || !dir.is_dir() {
            continue;
        }
        let manifest_path = dir.join("aoe-plugin.toml");
        let bytes = match std::fs::read(&manifest_path) {
            Ok(bytes) => bytes,
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

        // Non-fatal at load: an aoe upgrade must not brick startup.
        if let Err(msg) = manifest.host_compat(env!("CARGO_PKG_VERSION")) {
            load_errors.push(format!("plugin {id:?} at {}: {msg}", dir.display()));
            continue;
        }

        let plugin_config = config.plugins.get(&id);
        let source = plugin_config.and_then(|p| p.source.clone());
        let validation = validation_for(featured, &id, &dir, source.as_deref());

        if manifest.id.is_reserved_namespace() && validation != ValidationState::Featured {
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
        let enabled = plugin_config.map(|p| p.enabled).unwrap_or(true);
        let granted = plugin_config
            .and_then(|p| p.grant.as_ref())
            .map(|g| grant_covers(g, &manifest, &manifest_hash))
            .unwrap_or(false);

        plugins.push(LoadedPlugin {
            manifest,
            enabled,
            trust: TrustLevel::Community,
            validation,
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

        let stale = CapabilityGrant {
            manifest_hash: "sha256:old".to_string(),
            ..full.clone()
        };
        assert!(!grant_covers(&stale, &manifest, hash));

        let partial = CapabilityGrant {
            manifest_hash: hash.to_string(),
            capabilities: vec!["net".into()],
            granted_at: chrono::Utc::now(),
        };
        assert!(!grant_covers(&partial, &manifest, hash));
    }
}
