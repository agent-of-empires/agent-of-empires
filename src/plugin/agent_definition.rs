//! Borrowed host metadata projected from the validated declaration format.

use aoe_plugin_api::agent_integration::{
    AgentCaptureBackend, AgentCaptureContext, AgentHookEventDeclaration, AgentHookFormat,
    AgentHookIdentityField, AgentHookStatus, AgentIntegrationContribution, AgentKeyDeclaration,
    AgentResumeDeclaration, AgentYoloDeclaration,
};

use crate::agents::{
    AgentDef, AgentHookConfig, AgentLifecycle, DetectionMethod, ForkStrategy, HookEvent,
    HookFormat, HookIdentityField, HookStatus, KeyToken, PermissionResponse, ResumeStrategy,
    SessionCaptureBackend, SessionCaptureContext, SessionCaptureSpec, SessionSupport, ToolNames,
    YoloMode,
};

/// Reference arrays owned separately from the declaration, without leaking a registry snapshot.
pub(super) struct DefinitionData<'a> {
    aliases: Vec<&'a str>,
    environment: Vec<(&'a str, &'a str)>,
    allow: Vec<KeyToken<'a>>,
    always: Option<Vec<KeyToken<'a>>>,
    deny: Vec<KeyToken<'a>>,
    hook_events: Vec<HookEvent<'a>>,
}

fn hook_event(event: &AgentHookEventDeclaration) -> HookEvent<'_> {
    HookEvent {
        name: &event.event,
        matcher: event.matcher.as_deref(),
        status: event.status.map(|status| match status {
            AgentHookStatus::Running => HookStatus::Running,
            AgentHookStatus::Waiting => HookStatus::Waiting,
            AgentHookStatus::Idle => HookStatus::Idle,
            AgentHookStatus::Error => HookStatus::Error,
        }),
        identity_field: event.identity_field.map(|field| match field {
            AgentHookIdentityField::SessionId => HookIdentityField::SessionId,
            AgentHookIdentityField::ConversationIdOrSessionId => {
                HookIdentityField::ConversationIdOrSessionId
            }
        }),
        waiting_tools: ToolNames::Declared(&event.waiting_tools),
    }
}

impl<'a> DefinitionData<'a> {
    pub(super) fn new(entry: &'a AgentIntegrationContribution) -> Self {
        fn key(key: &AgentKeyDeclaration) -> KeyToken<'_> {
            match key {
                AgentKeyDeclaration::Literal { value } => KeyToken::Literal(value),
                AgentKeyDeclaration::Named { value } => KeyToken::Named(value),
            }
        }
        Self {
            aliases: entry
                .launch
                .iter()
                .flat_map(|launch| launch.aliases.iter().map(String::as_str))
                .collect(),
            environment: entry
                .launch
                .iter()
                .flat_map(|launch| {
                    launch
                        .container_env
                        .iter()
                        .map(|(key, value)| (key.as_str(), value.as_str()))
                })
                .collect(),
            allow: entry
                .reply_fallback
                .as_ref()
                .map_or_else(Vec::new, |reply| reply.allow.iter().map(key).collect()),
            always: entry.reply_fallback.as_ref().and_then(|reply| {
                reply
                    .allow_always
                    .as_ref()
                    .map(|keys| keys.iter().map(key).collect())
            }),
            deny: entry
                .reply_fallback
                .as_ref()
                .map_or_else(Vec::new, |reply| reply.deny.iter().map(key).collect()),
            hook_events: entry
                .hooks
                .iter()
                .flat_map(|hooks| hooks.events.iter().map(hook_event))
                .collect(),
        }
    }

    pub(super) fn definition<'b>(
        &'b self,
        entry: &'b AgentIntegrationContribution,
        status: bool,
    ) -> Option<AgentDef<'b>> {
        let command = entry.command.as_deref()?;
        let (prefix, _) = aoe_plugin_api::agent_integration::launch_prefix(command)?;
        let binary = *prefix.last()?;
        static DEFAULT_LAUNCH: std::sync::LazyLock<
            aoe_plugin_api::agent_integration::AgentLaunchDeclaration,
        > = std::sync::LazyLock::new(Default::default);
        let launch = entry.launch.as_ref().unwrap_or(&DEFAULT_LAUNCH);
        let yolo = launch.yolo.as_ref().map(|yolo| match yolo {
            AgentYoloDeclaration::Flag { flag } => YoloMode::CliFlag(flag),
            AgentYoloDeclaration::Always => YoloMode::AlwaysYolo,
            AgentYoloDeclaration::Env { key, value } => YoloMode::EnvVar(key, value),
        });
        let support = launch.resume.as_ref().map(|resume| {
            let resume = match resume {
                AgentResumeDeclaration::Flag { flag } => ResumeStrategy::Flag(flag),
                AgentResumeDeclaration::FlagPair {
                    existing,
                    new_session,
                } => ResumeStrategy::FlagPair {
                    existing,
                    new_session,
                },
                AgentResumeDeclaration::Subcommand { command } => {
                    ResumeStrategy::Subcommand(command)
                }
            };
            let capture = launch.capture.as_ref().and_then(|capture| {
                let context = |context| match context {
                    AgentCaptureContext::PaneScoped => Some(SessionCaptureContext::PaneScoped),
                    AgentCaptureContext::Unsupported => Some(SessionCaptureContext::Unsupported),
                    _ => None,
                };
                if capture.backend != AgentCaptureBackend::Omp {
                    return None;
                }
                Some(SessionCaptureSpec {
                    backend: SessionCaptureBackend::Omp,
                    host: context(capture.host)?,
                    sandbox: context(capture.sandbox)?,
                })
            });
            SessionSupport { resume, capture }
        });
        Some(AgentDef {
            name: &entry.tool,
            display_name: entry.display_name.as_deref().unwrap_or(&entry.tool),
            binary,
            launch_subcommand: None,
            aliases: &self.aliases,
            detection: DetectionMethod::Which(binary),
            yolo,
            spans_agent_generations: false,
            instruction_flag: launch.instruction_flag.as_deref(),
            oneshot_flag: launch.oneshot_flag.as_deref(),
            model_flag: launch.model_flag.as_deref(),
            set_default_command: false,
            detect_status: if status || entry.detection.is_some() {
                crate::tmux::status_detection::detect_native_only_status
            } else {
                crate::tmux::status_detection::detect_hook_only_status
            },
            container_env: &self.environment,
            hook_config: entry.hooks.as_ref().map(|hooks| AgentHookConfig {
                settings_rel_path: &hooks.settings_rel_path,
                config_dir_env_var: hooks.config_dir_env_var.as_deref(),
                events: &self.hook_events,
                strict: true,
                format: match hooks.format {
                    AgentHookFormat::JsonSettings => HookFormat::JsonSettings,
                },
            }),
            trust: entry.trust.as_ref(),
            sidecar_hooks: None,
            session_support: support,
            fork_strategy: ForkStrategy::Unsupported,
            fork_host_only: false,
            host_only: launch.host_only,
            send_keys_enter_delay_ms: launch.send_keys_enter_delay_ms.unwrap_or(0),
            ready_markers: &[],
            install_hint: launch.install_hint.as_deref().unwrap_or(&entry.tool),
            permission_response: entry.reply_fallback.as_ref().map(|_| PermissionResponse {
                allow: &self.allow,
                allow_always: self.always.as_deref(),
                deny: &self.deny,
            }),
            lifecycle: AgentLifecycle::Active,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors the built-in Claude table in `src/agents.rs`.
    const CLAUDE_SHAPED_HOOKS: &str = r#"
[agent_integrations.hooks]
settings_rel_path = ".claude/settings.json"
config_dir_env_var = "CLAUDE_CONFIG_DIR"
format = "json_settings"

[[agent_integrations.hooks.events]]
event = "SessionStart"
identity_field = "session_id"
[[agent_integrations.hooks.events]]
event = "PreToolUse"
status = "running"
waiting_tools = ["AskUserQuestion"]
[[agent_integrations.hooks.events]]
event = "PostToolUse"
matcher = "AskUserQuestion"
status = "running"
[[agent_integrations.hooks.events]]
event = "UserPromptSubmit"
status = "running"
identity_field = "session_id"
[[agent_integrations.hooks.events]]
event = "Stop"
status = "idle"
[[agent_integrations.hooks.events]]
event = "StopFailure"
status = "idle"
[[agent_integrations.hooks.events]]
event = "Notification"
matcher = "permission_prompt|elicitation_dialog|agent_needs_input"
status = "waiting"
[[agent_integrations.hooks.events]]
event = "Notification"
matcher = "idle_prompt|agent_completed"
status = "idle"
[[agent_integrations.hooks.events]]
event = "ElicitationResult"
status = "running"
"#;

    #[test]
    #[serial_test::serial]
    fn declared_claude_shaped_hooks_install_like_the_bundled_fallback_and_the_old_built_in() {
        let app = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(app.path());
        let manifest = aoe_plugin_api::PluginManifest::from_toml_str(&format!(
            "id = \"acme.claude-shaped\"\nname = \"Claude shaped\"\nversion = \"1.0.0\"\n\
             api_version = 28\n[[agent_integrations]]\ntool = \"example-helper\"\n\
             command = \"claude\"\n\
             [agent_integrations.launch.container_env]\nCLAUDE_CONFIG_DIR = \"/root/.claude\"\n\
             {CLAUDE_SHAPED_HOOKS}"
        ))
        .unwrap();
        let entry = &manifest.agent_integrations[0];
        let data = DefinitionData::new(entry);
        let declared = data.definition(entry, false).unwrap();
        let built_in = crate::agents::get_agent("claude").unwrap();
        let config = crate::session::config::Config::default();

        let dir = tempfile::tempdir().unwrap();
        let [from_declared, from_built_in] = ["declared.json", "built-in.json"].map(|name| {
            let path = dir.path().join(name);
            let agent = if name == "declared.json" {
                &declared
            } else {
                built_in
            };
            let events = crate::agents::resolved_hook_events(agent, &config).unwrap();
            crate::hooks::install_hooks(&path, &events, crate::hooks::HookInstallTarget::Host)
                .unwrap();
            std::fs::read(path).unwrap()
        });
        assert_eq!(from_declared, from_built_in);
        // Golden from the removed built-in Claude hook table (CLAUDE_HOOK_EVENTS).
        let golden = serde_json::json!({
            "SessionStart": [{"matcher": null, "hooks": [["identity"]]}],
            "PreToolUse": [{"matcher": null, "hooks": [["running", "waiting", "AskUserQuestion"]]}],
            "PostToolUse": [{"matcher": "AskUserQuestion", "hooks": [["running"]]}],
            "UserPromptSubmit": [{"matcher": null, "hooks": [["identity"], ["running"]]}],
            "Stop": [{"matcher": null, "hooks": [["idle"]]}],
            "StopFailure": [{"matcher": null, "hooks": [["idle"]]}],
            "Notification": [
                {"matcher": "permission_prompt|elicitation_dialog|agent_needs_input", "hooks": [["waiting"]]},
                {"matcher": "idle_prompt|agent_completed", "hooks": [["idle"]]}
            ],
            "ElicitationResult": [{"matcher": null, "hooks": [["running"]]}],
        });
        assert_eq!(hook_summary(&from_declared), golden);
        assert_eq!(hook_summary(&from_built_in), golden);
    }

    /// Per event and matcher, what each installed hook does: the status it
    /// writes, the waiting tool it guards, or `identity` for the session-id
    /// publisher. Independent of the uid-bearing shell text.
    fn hook_summary(installed: &[u8]) -> serde_json::Value {
        let settings: serde_json::Value = serde_json::from_slice(installed).unwrap();
        let mut summary = serde_json::Map::new();
        for (event, groups) in settings["hooks"].as_object().unwrap() {
            let groups: Vec<_> = groups
                .as_array()
                .unwrap()
                .iter()
                .map(|group| {
                    let hooks: Vec<_> = group["hooks"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|hook| {
                            let command = hook["command"].as_str().unwrap();
                            let mut tags = Vec::new();
                            if command.contains("__extract-session-id --field session-id") {
                                tags.push("identity".to_string());
                            }
                            for status in ["running", "waiting", "idle"] {
                                if command.contains(&format!("printf {status} >"))
                                    || command.contains(&format!("S={status}"))
                                {
                                    tags.push(status.to_string());
                                }
                            }
                            if command.contains("AskUserQuestion") {
                                tags.push("AskUserQuestion".to_string());
                            }
                            tags
                        })
                        .collect();
                    serde_json::json!({ "matcher": group.get("matcher"), "hooks": hooks })
                })
                .collect();
            summary.insert(event.clone(), groups.into());
        }
        summary.into()
    }
}
