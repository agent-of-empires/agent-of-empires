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
    /// First-party builtin (always true in the current core; installed plugins
    /// return in a follow-up).
    pub builtin: bool,
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
            builtin: true,
        }
    }
}
