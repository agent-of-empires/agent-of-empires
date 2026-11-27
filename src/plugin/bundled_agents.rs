//! Offline agent contracts, not installed plugins or implicit capability grants.
//!
//! Only an active, approved declaration can supersede these defaults.
//! Pane parsing stays host-side so disabling/uninstalling a plugin is reversible.

use std::sync::LazyLock;

use crate::agents::{
    AgentDef, ForkStrategy, ResumeStrategy, SessionCaptureBackend, SessionCaptureContext,
    SessionCaptureSpec, SessionSupport,
};

use super::agent_definition::DefinitionData;

fn manifest(source: &str, tool: &str) -> aoe_plugin_api::PluginManifest {
    let manifest = aoe_plugin_api::PluginManifest::from_toml_str(source)
        .unwrap_or_else(|e| panic!("bundled {tool} fallback declaration must validate: {e}"));
    assert_eq!(manifest.agent_integrations.len(), 1);
    assert_eq!(manifest.agent_integrations[0].tool, tool);
    manifest
}

static CLAUDE_MANIFEST: LazyLock<aoe_plugin_api::PluginManifest> =
    LazyLock::new(|| manifest(include_str!("../../assets/agents/claude.toml"), "claude"));
static CLAUDE_DATA: LazyLock<DefinitionData<'static>> =
    LazyLock::new(|| DefinitionData::new(&CLAUDE_MANIFEST.agent_integrations[0]));
/// The declaration lacks the pane detector, fork, ready markers, non-strict hook
/// installation and session support: validation accepts `resume` only with OMP capture.
static CLAUDE: LazyLock<AgentDef<'static>> = LazyLock::new(|| {
    let mut definition = CLAUDE_DATA
        .definition(&CLAUDE_MANIFEST.agent_integrations[0], false)
        .expect("bundled Claude fallback must declare a command");
    definition.detect_status = crate::tmux::status_detection::detect_claude_status;
    definition.fork_strategy = ForkStrategy::ClaudeFork;
    definition.ready_markers = &[
        "shift+tab to cycle",
        "manual mode on",
        "for shortcuts",
        "for agents",
    ];
    if let Some(hooks) = definition.hook_config.as_mut() {
        hooks.strict = false;
    }
    definition.session_support = Some(SessionSupport {
        resume: ResumeStrategy::FlagPair {
            existing: "--resume",
            new_session: "--session-id",
        },
        capture: Some(SessionCaptureSpec {
            backend: SessionCaptureBackend::Claude,
            host: SessionCaptureContext::PaneScoped,
            sandbox: SessionCaptureContext::PaneScoped,
        }),
    });
    definition
});

static OMP_MANIFEST: LazyLock<aoe_plugin_api::PluginManifest> =
    LazyLock::new(|| manifest(include_str!("../../assets/agents/omp.toml"), "omp"));
static OMP_DATA: LazyLock<DefinitionData<'static>> =
    LazyLock::new(|| DefinitionData::new(&OMP_MANIFEST.agent_integrations[0]));
static OMP: LazyLock<AgentDef<'static>> = LazyLock::new(|| {
    let mut definition = OMP_DATA
        .definition(&OMP_MANIFEST.agent_integrations[0], false)
        .expect("bundled OMP fallback must declare a command");
    definition.display_name = "Oh My Pi";
    definition.detect_status = crate::tmux::status_detection::detect_omp_status;
    definition.fork_strategy = ForkStrategy::ValueFlag("--fork");
    definition.fork_host_only = true;
    definition
});

pub(crate) fn contains(tool: &str) -> bool {
    [&*CLAUDE_MANIFEST, &*OMP_MANIFEST]
        .iter()
        .flat_map(|manifest| &manifest.agent_integrations)
        .any(|entry| entry.tool == tool)
}

/// Static fallback metadata; callers decide whether an approved declaration takes over.
pub(crate) fn definitions() -> impl Iterator<Item = &'static AgentDef<'static>> {
    [&*CLAUDE, &*OMP].into_iter()
}

/// The fallback identity for `tool`, even once an approved declaration has
/// taken over its listing. A session stored with the fallback's own command
/// keeps that native identity, and its pane detector stays available.
pub(crate) fn definition(tool: &str) -> Option<&'static AgentDef<'static>> {
    definitions().find(|agent| agent.name == tool)
}

/// Whether an approved declaration for the bundled `tool` supersedes its fallback.
pub(crate) fn taken_over(registry: &std::sync::Arc<super::PluginRegistry>, tool: &str) -> bool {
    contains(tool)
        && super::agent_integration::declared_agent(registry.clone(), tool, std::iter::empty())
            .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fallback is the only Claude definition: it must keep the launch,
    /// resume, hook and trust contract the built-in entry used to carry.
    #[test]
    fn claude_fallback_carries_the_launch_contract() {
        let claude = definition("claude").unwrap();

        assert_eq!((claude.name, claude.binary), ("claude", "claude"));
        assert_eq!(claude.oneshot_flag, Some("-p"));
        assert_eq!(claude.instruction_flag, Some("--append-system-prompt {}"));
        assert!(matches!(
            &claude.yolo,
            Some(crate::agents::YoloMode::CliFlag(
                "--dangerously-skip-permissions"
            ))
        ));
        assert_eq!(
            claude.container_env,
            &[("CLAUDE_CONFIG_DIR", "/root/.claude")]
        );
        assert_eq!(claude.send_keys_enter_delay_ms, 150);
        assert_eq!(claude.fork_strategy, ForkStrategy::ClaudeFork);
        assert!(claude.sidecar_hooks.is_none());
        assert_eq!(
            claude.session_support.map(|support| support.resume),
            Some(ResumeStrategy::FlagPair {
                existing: "--resume",
                new_session: "--session-id",
            })
        );

        let hooks = claude.hook_config.as_ref().unwrap();
        assert_eq!(hooks.settings_rel_path, ".claude/settings.json");
        assert_eq!(hooks.config_dir_env_var, Some("CLAUDE_CONFIG_DIR"));
        assert!(!hooks.strict);
        let events: Vec<_> = hooks.events.iter().map(|event| event.name).collect();
        assert_eq!(
            events,
            [
                "SessionStart",
                "PreToolUse",
                "PostToolUse",
                "UserPromptSubmit",
                "Stop",
                "StopFailure",
                "Notification",
                "Notification",
                "ElicitationResult",
            ]
        );

        let trust = claude.trust.expect("declared trust");
        assert_eq!(trust.file, ".claude.json");
        assert_eq!(
            trust.config_dir_env_var.as_deref(),
            Some("CLAUDE_CONFIG_DIR")
        );
        assert_eq!(trust.pointer, "/projects/{path}/hasTrustDialogAccepted");
        assert_eq!(
            trust.value,
            aoe_plugin_api::agent_integration::AgentTrustValue::Bool(true)
        );
        assert!(crate::hooks::declared_trust_applies_in_sandbox(
            trust, false
        ));
    }

    #[test]
    fn lookups_cover_both_fallbacks() {
        assert!(contains("claude") && contains("omp") && !contains("codex"));
        assert!(definition("claude").is_some() && definition("omp").is_some());
        assert!(definition("codex").is_none());
    }
}
