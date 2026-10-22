//! Plugin manifest types for the Agent of Empires plugin system.

pub mod acp;
mod capability;
mod id;
mod manifest;
pub mod session;

pub use capability::{CapabilityId, TrustLevel, KNOWN_CAPABILITIES};
pub use id::{InvalidPluginId, PluginId};
pub use manifest::{
    lucide_icon_name_ok, page_entry_path_ok, screenshot_path_ok, BuildStep, ClientAction,
    CommandContribution, HooksSpec, KeybindContribution, ManifestError, McpRewriteHook,
    McpRewriteScope, ObjectFieldContribution, ObjectFieldType, OptionSource, PluginManifest,
    RuntimeSpec, Screenshot, SettingContribution, SettingType, StatusContribution,
    ThemeContribution, UiContribution, UiSlot, MAX_SCREENSHOTS, MCP_REWRITE_DEFAULT_TIMEOUT_MS,
    MCP_REWRITE_MAX_TIMEOUT_MS,
};

pub const API_VERSION: u32 = 19;
