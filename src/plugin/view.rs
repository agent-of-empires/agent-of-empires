//! The shared plugin view-model: one Rust description of a plugin that both the

use serde::Serialize;

use super::registry::LoadedPlugin;

#[derive(Debug, Clone, Serialize)]
pub struct PluginView {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub icon: Option<String>,
    pub icon_asset_url: Option<String>,
    pub enabled: bool,
    pub builtin: bool,
    pub validation: String,
    pub source: Option<String>,
    pub capabilities: Vec<String>,
    pub ui_contributions: Vec<UiContributionView>,
    pub granted: bool,
    pub needs_reapproval: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct UiContributionView {
    pub slot: String,
    pub id: String,
}

impl LoadedPlugin {
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
