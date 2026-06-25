//! Plugin manifest types for the Agent of Empires plugin system.
//!
//! This crate is the stable surface a plugin author (and the in-tree host)
//! compiles against: the `aoe-plugin.toml` manifest schema, the capability
//! taxonomy, and the validation rules that gate a manifest before it loads.
//! The contribution sections (capabilities, commands, keybinds, settings,
//! themes, ui, runtime worker) are defined here. Settings and themes are
//! consumed by the Tier 0 registries (#2094); keybinds/commands resolve and
//! graft at Tier 0 but execute only with the runtime host (#2095); ui slots
//! land with #2366. Status and panes are deferred until a consumer exists
//! (#2386). See `docs/development/internals/plugin-system.md`.

mod capability;
mod id;
mod manifest;

pub use capability::{CapabilityId, TrustLevel, KNOWN_CAPABILITIES};
pub use id::{InvalidPluginId, PluginId};
pub use manifest::{
    BuildStep, CommandContribution, KeybindContribution, ManifestError, PluginManifest,
    RuntimeSpec, SettingContribution, SettingType, ThemeContribution, UiContribution, UiSlot,
};

/// Version of the manifest schema and host API this crate describes.
///
/// A manifest declares the `api_version` it was written against; the host
/// refuses manifests targeting a newer version than it understands. Bumped to
/// 2 when the contribution sections and capability taxonomy were added.
pub const API_VERSION: u32 = 2;
