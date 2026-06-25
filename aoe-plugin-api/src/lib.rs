//! Plugin manifest types for the Agent of Empires plugin system.
//!
//! This crate is the stable surface a plugin author (and the in-tree host)
//! compiles against: the `aoe-plugin.toml` manifest schema, the capability
//! taxonomy, and the validation rules that gate a manifest before it loads.
//! The contribution sections (capabilities, commands, keybinds, settings, ui,
//! runtime worker) are defined here but consumed by follow-up PRs (#2094 /
//! #2095 / #2366). Themes, status, and panes are deferred until a consumer
//! exists. See `docs/development/internals/plugin-system.md`.

mod capability;
mod id;
mod manifest;

pub use capability::{CapabilityId, TrustLevel, KNOWN_CAPABILITIES};
pub use id::{InvalidPluginId, PluginId};
pub use manifest::{
    CommandContribution, KeybindContribution, ManifestError, PluginManifest, RuntimeSpec,
    SettingContribution, UiContribution,
};

/// Version of the manifest schema and host API this crate describes.
///
/// A manifest declares the `api_version` it was written against; the host
/// refuses manifests targeting a newer version than it understands. Bumped to
/// 2 when the contribution sections and capability taxonomy were added.
pub const API_VERSION: u32 = 2;
