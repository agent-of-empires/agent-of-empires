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
/// Claude is listed by `AGENTS` until it leaves that table.
pub(crate) fn definitions() -> impl Iterator<Item = &'static AgentDef<'static>> {
    std::iter::once(&*OMP)
}

/// The fallback identity for `tool`, even once an approved declaration has
/// taken over its listing. A session stored with the fallback's own command
/// keeps that native identity, and its pane detector stays available.
pub(crate) fn definition(tool: &str) -> Option<&'static AgentDef<'static>> {
    [&*CLAUDE]
        .into_iter()
        .chain(definitions())
        .find(|agent| agent.name == tool)
}

pub(crate) fn available() -> impl Iterator<Item = &'static AgentDef<'static>> {
    let registry = super::registry();
    definitions().filter(move |agent| {
        super::agent_integration::declared_agent(registry.clone(), agent.name, std::iter::empty())
            .is_none()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bundled Claude fallback must stay indistinguishable from the built-in entry.
    #[test]
    fn claude_fallback_matches_the_built_in_entry() {
        let bundled = definition("claude").unwrap();
        let built_in = crate::agents::AGENTS
            .iter()
            .find(|agent| agent.name == "claude")
            .unwrap();

        assert_eq!(bundled.name, built_in.name);
        assert_eq!(bundled.binary, built_in.binary);
        assert_eq!(bundled.launch_subcommand, built_in.launch_subcommand);
        assert_eq!(bundled.aliases, built_in.aliases);
        assert!(matches!(
            (&bundled.yolo, &built_in.yolo),
            (
                Some(crate::agents::YoloMode::CliFlag(a)),
                Some(crate::agents::YoloMode::CliFlag(b)),
            ) if a == b
        ));
        assert_eq!(bundled.instruction_flag, built_in.instruction_flag);
        assert_eq!(bundled.oneshot_flag, built_in.oneshot_flag);
        assert_eq!(bundled.model_flag, built_in.model_flag);
        assert_eq!(bundled.container_env, built_in.container_env);
        assert_eq!(bundled.session_support, built_in.session_support);
        assert_eq!(bundled.fork_strategy, built_in.fork_strategy);
        assert_eq!(bundled.fork_host_only, built_in.fork_host_only);
        assert_eq!(bundled.host_only, built_in.host_only);
        assert_eq!(
            bundled.spans_agent_generations,
            built_in.spans_agent_generations
        );
        assert_eq!(bundled.set_default_command, built_in.set_default_command);
        assert_eq!(bundled.ready_markers, built_in.ready_markers);
        assert_eq!(
            bundled.send_keys_enter_delay_ms,
            built_in.send_keys_enter_delay_ms
        );
        assert_eq!(bundled.install_hint, built_in.install_hint);
        assert_eq!(bundled.lifecycle, built_in.lifecycle);
        assert_eq!(
            format!("{:?}", bundled.permission_response),
            format!("{:?}", built_in.permission_response)
        );
        assert!(bundled.sidecar_hooks.is_none() && built_in.sidecar_hooks.is_none());

        let (a, b) = (
            bundled.hook_config.as_ref().unwrap(),
            built_in.hook_config.as_ref().unwrap(),
        );
        assert_eq!(a.settings_rel_path, b.settings_rel_path);
        assert_eq!(a.config_dir_env_var, b.config_dir_env_var);
        assert_eq!(a.format, b.format);
        assert!(!a.strict && !b.strict);
        assert_eq!(a.events.len(), b.events.len());
        for (x, y) in a.events.iter().zip(b.events) {
            assert_eq!(
                (x.name, x.matcher, x.status, x.identity_field),
                (y.name, y.matcher, y.status, y.identity_field)
            );
            assert_eq!(
                x.waiting_tools.iter().collect::<Vec<_>>(),
                y.waiting_tools.iter().collect::<Vec<_>>()
            );
        }

        // The built-in writers carry trust by name; the declaration names the same record.
        let trust = bundled.trust.expect("declared trust");
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
    fn claude_fallback_behaves_like_the_built_in_entry() {
        let bundled = definition("claude").unwrap();
        let built_in = crate::agents::AGENTS
            .iter()
            .find(|agent| agent.name == "claude")
            .unwrap();

        for pane in [
            "",
            "Do you want to proceed?\n 1. Yes\n 2. No",
            "esc to interrupt",
            "? for shortcuts",
            "✻ Thinking… (esc to interrupt)",
        ] {
            assert_eq!(
                (bundled.detect_status)(pane),
                (built_in.detect_status)(pane),
                "{pane:?}"
            );
        }

        let config = crate::session::config::Config::default();
        let dir = tempfile::tempdir().unwrap();
        let [from_bundled, from_built_in] =
            [(bundled, "bundled.json"), (built_in, "built-in.json")].map(|(agent, name)| {
                let path = dir.path().join(name);
                let events = crate::agents::resolved_hook_events(agent, &config).unwrap();
                crate::hooks::install_hooks(&path, &events, crate::hooks::HookInstallTarget::Host)
                    .unwrap();
                std::fs::read(path).unwrap()
            });
        assert!(!from_built_in.is_empty());
        assert_eq!(from_bundled, from_built_in);
    }

    #[test]
    fn lookups_cover_both_fallbacks() {
        assert!(contains("claude") && contains("omp") && !contains("codex"));
        assert!(definition("claude").is_some() && definition("omp").is_some());
        assert!(definition("codex").is_none());
    }
}
