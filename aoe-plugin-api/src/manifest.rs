use serde::{Deserialize, Serialize};

use crate::{PluginId, API_VERSION};

/// Parsed `aoe-plugin.toml`.
///
/// This is the minimal core schema: a plugin's identity and the manifest/API
/// version it targets. Contribution sections (settings, keybinds, themes,
/// commands, status detection, UI slots, panes, runtime workers) return in
/// follow-up PRs; see `docs/development/internals/plugin-system.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct PluginManifest {
    pub id: PluginId,
    /// Human-readable display name.
    pub name: String,
    pub version: String,
    /// Manifest schema / host API version this manifest targets.
    pub api_version: u32,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ManifestError {
    #[error("manifest is not valid TOML: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("manifest targets api_version {found} but this host supports 1..={max}; upgrade aoe")]
    UnsupportedApiVersion { found: u64, max: u32 },
    #[error("manifest is invalid:\n{}", .0.join("\n"))]
    Invalid(Vec<String>),
}

impl PluginManifest {
    /// Parse and validate an `aoe-plugin.toml` document.
    pub fn from_toml_str(input: &str) -> Result<Self, ManifestError> {
        // Pre-parse api_version permissively first. A manifest targeting a
        // newer host may introduce fields this host's strict schema does not
        // know, so a plain `toml::from_str::<Self>` would fail with a confusing
        // "unknown field" error. Surfacing the version mismatch first tells the
        // author the real problem (upgrade aoe).
        if let Some(found) = toml::from_str::<toml::Value>(input)
            .ok()
            .and_then(|doc| doc.get("api_version").and_then(toml::Value::as_integer))
        {
            if found > API_VERSION as i64 {
                return Err(ManifestError::UnsupportedApiVersion {
                    found: found as u64,
                    max: API_VERSION,
                });
            }
        }
        let manifest: Self = toml::from_str(input)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Structural validation; collects every problem instead of stopping at
    /// the first so a plugin author sees the full list in one pass.
    pub fn validate(&self) -> Result<(), ManifestError> {
        let mut errors = Vec::new();
        let mut check = |ok: bool, msg: String| {
            if !ok {
                errors.push(msg);
            }
        };

        check(
            (1..=API_VERSION).contains(&self.api_version),
            format!(
                "api_version {} is not supported (host supports 1..={API_VERSION})",
                self.api_version
            ),
        );
        check(!self.version.is_empty(), "version must not be empty".into());
        check(!self.name.is_empty(), "name must not be empty".into());

        if errors.is_empty() {
            Ok(())
        } else {
            Err(ManifestError::Invalid(errors))
        }
    }
}
