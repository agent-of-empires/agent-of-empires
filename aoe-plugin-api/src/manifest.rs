use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{CapabilityId, PluginId, API_VERSION};

/// Parsed `aoe-plugin.toml`.
///
/// Identity (`id`, `name`, `version`, `api_version`, `description`) plus the
/// contribution sections a plugin declares. The contribution sections are
/// defined here but consumed by later issues: the settings registry (#2094),
/// the runtime host (#2095), and the command/keybind/UI surfaces (#2366). This
/// host parses and validates them; it does not yet act on them.
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

    /// Resource/effect capabilities the plugin requests. Static contributions
    /// below are NOT listed here; only runtime resource access is. The user
    /// grants these once at install (community plugins); builtins are
    /// auto-granted. See [`crate::capability`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<CapabilityId>,

    /// Commands the plugin contributes (palette / CLI). Consumed by #2366.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<CommandContribution>,

    /// Keybinds the plugin contributes. Consumed by #2366.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keybinds: Vec<KeybindContribution>,

    /// Settings the plugin declares. The typed schema that validates and
    /// renders them lands with #2094.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub settings: Vec<SettingContribution>,

    /// UI slots the plugin renders into. Consumed by #2366.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ui: Vec<UiContribution>,

    /// The worker entrypoint. Defined here so installation can fetch a
    /// release-binary worker; actually launching it is #2095.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeSpec>,
}

/// A command the plugin contributes. The host namespaces it as
/// `plugin.<plugin-id>.<id>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandContribution {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
}

/// A keybind the plugin contributes, binding a key chord to a command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeybindContribution {
    /// Command id this binds to (a plugin command or a core command).
    pub command: String,
    /// Key chord, e.g. `Ctrl+K`. Parsed by the consuming surface (#2366).
    pub key: String,
}

/// A setting the plugin declares. The typed schema arrives with #2094; here it
/// is just the key and human-facing labels.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingContribution {
    pub key: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub description: String,
}

/// A UI contribution targeting a named slot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiContribution {
    pub slot: String,
    #[serde(default)]
    pub id: String,
}

/// How the plugin's worker is launched. Defined here; executed by #2095.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum RuntimeSpec {
    /// A worker launched by running a command from the plugin directory (a
    /// script, an interpreter invocation, or an in-tree binary).
    Command {
        /// argv; the first element is the program, the rest its arguments.
        command: Vec<String>,
    },
    /// A worker binary downloaded from the source repo's GitHub release assets.
    /// Installation resolves the asset for the host platform, downloads it, and
    /// places the binary in the plugin directory.
    ReleaseBinary {
        /// Asset name template. `${os}`, `${arch}`, and `${target}` are
        /// substituted with the host's values before matching the release.
        asset: String,
        /// Executable to run after extraction (the path within an archive). The
        /// downloaded asset itself when omitted (a raw, non-archive binary).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bin: Option<String>,
    },
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

    /// sha256 over the raw `aoe-plugin.toml` bytes as installed, formatted
    /// `sha256:<hex>`. A capability grant is pinned to this; an update whose
    /// manifest bytes (hence possibly its capability set) change re-prompts.
    /// Hashing the raw bytes, not a reserialized struct, avoids depending on
    /// serializer canonicalization.
    pub fn hash_bytes(bytes: &[u8]) -> String {
        use std::fmt::Write;
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let digest = hasher.finalize();
        let mut out = String::with_capacity(7 + digest.len() * 2);
        out.push_str("sha256:");
        for byte in digest {
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    /// Structural validation; collects every problem instead of stopping at
    /// the first so a plugin author sees the full list in one pass.
    ///
    /// Capability strings are deliberately not validated here: they are open
    /// strings (forward-compatible), and the host rejects an unknown one at
    /// install rather than at parse, so a manifest targeting a newer host still
    /// parses on an older one.
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

        if let Some(RuntimeSpec::Command { command }) = &self.runtime {
            check(
                !command.is_empty(),
                "runtime command must not be empty".into(),
            );
            check(
                command.iter().all(|arg| !arg.is_empty()),
                "runtime command must not contain empty arguments".into(),
            );
        }
        if let Some(RuntimeSpec::ReleaseBinary { asset, bin }) = &self.runtime {
            check(
                !asset.is_empty(),
                "runtime release-binary asset must not be empty".into(),
            );
            check(
                bin.as_ref().map(|b| !b.is_empty()).unwrap_or(true),
                "runtime release-binary bin must not be empty".into(),
            );
        }

        // Contribution sections declare required identifiers; an empty one would
        // install and persist a malformed manifest, so reject it here rather
        // than push the cleanup onto the later consumers (#2094 / #2095 / #2366).
        for (i, c) in self.commands.iter().enumerate() {
            check(
                !c.id.is_empty(),
                format!("commands[{i}].id must not be empty"),
            );
        }
        for (i, k) in self.keybinds.iter().enumerate() {
            check(
                !k.command.is_empty(),
                format!("keybinds[{i}].command must not be empty"),
            );
            check(
                !k.key.is_empty(),
                format!("keybinds[{i}].key must not be empty"),
            );
        }
        for (i, s) in self.settings.iter().enumerate() {
            check(
                !s.key.is_empty(),
                format!("settings[{i}].key must not be empty"),
            );
        }
        for (i, u) in self.ui.iter().enumerate() {
            check(
                !u.slot.is_empty(),
                format!("ui[{i}].slot must not be empty"),
            );
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(ManifestError::Invalid(errors))
        }
    }
}
