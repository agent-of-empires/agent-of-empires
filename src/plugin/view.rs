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
    pub enabled: bool,
    /// First-party builtin (compiled in) versus an externally installed plugin.
    pub builtin: bool,
    /// `builtin` or `community`.
    pub trust: String,
    /// Install source for an external plugin (`gh:owner/repo` or a path).
    pub source: Option<String>,
    /// Capabilities the plugin's manifest declares.
    pub capabilities: Vec<String>,
    /// Whether the user's grant covers the installed manifest (always true for
    /// builtins).
    pub granted: bool,
    /// Installed but inactive: a community plugin awaiting capability approval.
    pub needs_reapproval: bool,
}

impl LoadedPlugin {
    /// The view-model for this plugin: the single shape both UIs render from.
    pub fn view(&self) -> PluginView {
        PluginView {
            id: self.id().to_string(),
            name: self.manifest.name.clone(),
            version: self.manifest.version.clone(),
            description: self.manifest.description.clone(),
            enabled: self.enabled,
            builtin: self.builtin(),
            trust: self.trust.as_str().to_string(),
            source: self.source.clone(),
            capabilities: self
                .manifest
                .capabilities
                .iter()
                .map(|c| c.as_str().to_string())
                .collect(),
            granted: self.granted,
            needs_reapproval: self.needs_reapproval(),
        }
    }
}
