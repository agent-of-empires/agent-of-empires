//! Plugin manifest types for the Agent of Empires plugin system.
//!
//! Defines `aoe-plugin.toml`, capabilities, session RPC payloads, and manifest
//! validation without depending on the host crate.

pub mod acp;
mod capability;
mod id;
mod manifest;
pub mod session;

pub use capability::{CapabilityId, TrustLevel, KNOWN_CAPABILITIES};
pub use id::{InvalidPluginId, PluginId};
pub use manifest::{
    lucide_icon_name_ok, screenshot_path_ok, BuildStep, ClientAction, CommandContribution,
    KeybindContribution, ManifestError, ObjectFieldContribution, ObjectFieldType, OptionSource,
    PluginManifest, RuntimeSpec, Screenshot, SettingContribution, SettingType, StatusContribution,
    ThemeContribution, UiContribution, UiSlot, MAX_SCREENSHOTS,
};

/// Current manifest schema and host API version. The host rejects newer
/// manifests. Version history is documented in `docs/plugin-api.md`.
pub const API_VERSION: u32 = 13;
