//! Plugin manifest types for the Agent of Empires plugin system.

pub mod acp;
pub mod agent_integration;
mod capability;
mod id;
mod manifest;
mod mcp;
pub mod session;

pub use capability::{CapabilityId, TrustLevel, KNOWN_CAPABILITIES};
pub use id::{InvalidPluginId, PluginId};
pub use manifest::{
    deep_link_value_ok, lucide_icon_name_ok, page_entry_path_ok, screenshot_path_ok, BuildStep,
    ClientAction, CommandContribution, HooksSpec, KeybindContribution, ManifestError,
    McpRewriteHook, McpRewriteScope, ObjectFieldContribution, ObjectFieldType, OptionSource,
    PluginManifest, RuntimeSpec, Screenshot, SettingContribution, SettingType, StatusContribution,
    ThemeContribution, UiContribution, UiSlot, MAX_SCREENSHOTS, MCP_REWRITE_DEFAULT_TIMEOUT_MS,
    MCP_REWRITE_MAX_TIMEOUT_MS,
};

pub use mcp::{
    is_valid_mcp_server_name, mcp_prefix, MCP_SERVE_CAPABILITY, MCP_SERVE_DEFAULT_TIMEOUT_MS,
    MCP_SERVE_MAX_PER_PLUGIN, MCP_SERVE_MAX_TIMEOUT_MS,
};

pub const API_VERSION: u32 = 22;
