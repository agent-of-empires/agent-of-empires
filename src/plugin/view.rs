//! The shared plugin view-model: one Rust description of a plugin that both the
//! web dashboard (serialized over `GET /api/plugins`) and the native TUI
//! render from, so neither re-derives the shape.

use serde::Serialize;

use super::registry::LoadedPlugin;

/// The manager's view of one plugin. Built by [`LoadedPlugin::view`], consumed
/// directly by the TUI and serialized for the web (the `GET /api/plugins`
/// contract the web TypeScript mirrors).
#[derive(Debug, Clone, Serialize)]
pub struct PluginView {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    /// Lucide kebab-case identity icon name, straight from the manifest.
    pub icon: Option<String>,
    /// Resolved URL for the manifest's `icon_asset`, only set when the plugin
    /// has both an on-disk install directory (not a builtin) and an
    /// `icon_asset` path: `GET /api/plugins/{id}/icon` streams it from the
    /// install directory.
    pub icon_asset_url: Option<String>,
    pub enabled: bool,
    /// First-party builtin (compiled in) versus an externally installed plugin.
    pub builtin: bool,
    /// Validation provenance: `builtin`, `featured`, `community`, or `local`.
    pub validation: String,
    /// Install source for an external plugin (`gh:owner/repo` or a path).
    pub source: Option<String>,
    /// Capabilities the plugin's manifest declares.
    pub capabilities: Vec<String>,
    /// UI slots the plugin declares it will render into (#2366). Disclosed
    /// alongside capabilities so a surface can show the user that the plugin
    /// modifies the dashboard, even though a UI contribution needs no grant.
    pub ui_contributions: Vec<UiContributionView>,
    /// Whether the user's grant covers the installed manifest (always true for
    /// builtins).
    pub granted: bool,
    /// Installed but inactive: a community plugin awaiting capability approval.
    pub needs_reapproval: bool,
}

/// A declared UI contribution, flattened for display: the kebab-case slot name
/// and the plugin-chosen entry id.
#[derive(Debug, Clone, Serialize)]
pub struct UiContributionView {
    pub slot: String,
    pub id: String,
}

impl LoadedPlugin {
    /// The view-model for this plugin: the single shape both UIs render from.
    pub fn view(&self) -> PluginView {
        PluginView {
            id: self.id().to_string(),
            name: self.manifest.name.clone(),
            version: self.manifest.version.clone(),
            description: self.manifest.description.clone(),
            icon: self.manifest.icon.clone(),
            icon_asset_url: (self.manifest.icon_asset.is_some() && self.dir.is_some())
                .then(|| format!("/api/plugins/{}/icon", self.id())),
            enabled: self.enabled,
            builtin: self.builtin(),
            validation: self.validation.as_str().to_string(),
            source: self.source.clone(),
            capabilities: self
                .manifest
                .capabilities
                .iter()
                .map(|c| c.as_str().to_string())
                .collect(),
            ui_contributions: self
                .manifest
                .ui
                .iter()
                .map(|u| UiContributionView {
                    slot: u.slot.as_str().to_string(),
                    id: u.id.clone(),
                })
                .collect(),
            granted: self.granted,
            needs_reapproval: self.needs_reapproval(),
        }
    }
}
