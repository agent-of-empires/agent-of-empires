//! Plugin manifest types for the Agent of Empires plugin system.
//!
//! This crate is the stable surface a plugin author (and the in-tree host)
//! compiles against: the `aoe-plugin.toml` manifest schema, the capability
//! taxonomy, and the validation rules that gate a manifest before it loads.
//! The contribution sections (capabilities, commands, keybinds, settings,
//! themes, ui, runtime worker) are defined here. Settings and themes are
//! consumed by the Tier 0 registries (#2094); keybinds/commands resolve and
//! graft at Tier 0 but execute only with the runtime host (#2095); ui slots
//! land with #2366; the status section's consumer is the status reference
//! plugin (#2096). Panes are not a manifest section: they ship as a `ui` slot
//! kind (#2432). See `docs/development/internals/plugin-system.md`.

mod capability;
mod id;
mod manifest;

pub use capability::{CapabilityId, TrustLevel, KNOWN_CAPABILITIES};
pub use id::{InvalidPluginId, PluginId};
pub use manifest::{
    screenshot_path_ok, BuildStep, ClientAction, CommandContribution, KeybindContribution,
    ManifestError, PluginManifest, RuntimeSpec, Screenshot, SettingContribution, SettingType,
    StatusContribution, ThemeContribution, UiContribution, UiSlot, MAX_SCREENSHOTS,
};

/// Version of the manifest schema and host API this crate describes.
///
/// A manifest declares the `api_version` it was written against; the host
/// refuses manifests targeting a newer version than it understands. Bumped to
/// 2 when the contribution sections and capability taxonomy were added; 3 when
/// the `detail-panel` slot became the dockable `pane` slot (with
/// `default_location`); 4 when the `status` contribution section and the
/// `aoe_version` host-compatibility field were added; 5 when the `screenshots`
/// presentation metadata was added; 6 when a command could declare a
/// client-executed `action` (`ClientAction`).
pub const API_VERSION: u32 = 6;
