//! Offline agent contracts, not installed plugins or implicit capability grants.
//!
//! Only an active, approved API21 declaration can supersede these defaults.
//! Pane parsing stays host-side so disabling/uninstalling a plugin is reversible.

use std::sync::LazyLock;

use crate::agents::AgentDef;

use super::agent_definition::DefinitionData;

static MANIFEST: LazyLock<aoe_plugin_api::PluginManifest> = LazyLock::new(|| {
    aoe_plugin_api::PluginManifest::from_toml_str(include_str!("../../assets/agents/omp.toml"))
        .expect("bundled OMP fallback declaration must validate")
});
static DATA: LazyLock<DefinitionData<'static>> =
    LazyLock::new(|| DefinitionData::new(&MANIFEST.agent_integrations[0]));
static DEFINITION: LazyLock<AgentDef<'static>> = LazyLock::new(|| {
    let mut definition = DATA
        .definition(&MANIFEST.agent_integrations[0], false)
        .expect("bundled OMP fallback must declare a command");
    definition.detect_status = crate::tmux::status_detection::detect_omp_status;
    definition
});

pub(crate) fn contains(tool: &str) -> bool {
    MANIFEST.agent_integrations.iter().any(|entry| entry.tool == tool)
}

/// Static fallback metadata; callers decide whether an approved declaration takes over.
pub(crate) fn definitions() -> impl Iterator<Item = &'static AgentDef<'static>> {
    std::iter::once(&*DEFINITION)
}

pub(crate) fn available() -> impl Iterator<Item = &'static AgentDef<'static>> {
    let registry = super::registry();
    definitions().filter(move |agent| {
        super::agent_integration::declared_agent(
            registry.clone(), agent.name, std::iter::empty(),
        ).is_none()
    })
}
