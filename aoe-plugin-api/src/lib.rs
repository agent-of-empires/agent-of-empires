//! Plugin manifest types for the Agent of Empires plugin system.

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

pub const API_VERSION: u32 = 13;
