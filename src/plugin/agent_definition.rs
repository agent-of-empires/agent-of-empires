//! Borrowed host metadata projected from the validated declaration format.

use aoe_plugin_api::agent_integration::{
    AgentCaptureBackend, AgentCaptureContext, AgentIntegrationContribution, AgentKeyDeclaration,
    AgentResumeDeclaration, AgentYoloDeclaration,
};

use crate::agents::{
    AgentDef, AgentLifecycle, DetectionMethod, ForkStrategy, KeyToken, PermissionResponse,
    ResumeStrategy, SessionCaptureBackend, SessionCaptureContext, SessionCaptureSpec,
    SessionSupport, YoloMode,
};

/// Reference arrays owned separately from the declaration, without leaking a registry snapshot.
pub(super) struct DefinitionData<'a> {
    aliases: Vec<&'a str>,
    environment: Vec<(&'a str, &'a str)>,
    allow: Vec<KeyToken<'a>>,
    always: Option<Vec<KeyToken<'a>>>,
    deny: Vec<KeyToken<'a>>,
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
            hook_config: None,
            sidecar_hooks: None,
            session_support: support,
            fork_strategy: ForkStrategy::Unsupported,
            host_only: launch.host_only,
            send_keys_enter_delay_ms: 0,
            ready_marker: None,
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
