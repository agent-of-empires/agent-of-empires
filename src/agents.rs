//! Registry of supported agent CLIs and their per-agent metadata.

use crate::session::Status;
use crate::tmux::status_detection;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookStatus {
    Running,
    Waiting,
    Idle,
    Error,
}

impl HookStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            HookStatus::Running => "running",
            HookStatus::Waiting => "waiting",
            HookStatus::Idle => "idle",
            HookStatus::Error => "error",
        }
    }
}

impl std::fmt::Display for HookStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

pub enum DetectionMethod {
    Which(&'static str),
    RunWithArg(&'static str, &'static str),
}

pub enum YoloMode {
    CliFlag(&'static str),
    EnvVar(&'static str, &'static str),
    AlwaysYolo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeStrategy {
    Flag(&'static str),
    FlagPair {
        existing: &'static str,
        new_session: &'static str,
    },
    Subcommand(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCaptureBackend {
    Claude,
    OpenCode,
    Codex,
    Gemini,
    HookSidecar,
    Pi,
    Hermes,
    Kimi,
    Omp,
    PrimeAgent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionIdentityPublisher {
    Extension { root_only: bool },
}

impl SessionCaptureBackend {
    pub(crate) const fn identity_publisher(self) -> Option<SessionIdentityPublisher> {
        match self {
            Self::Pi => Some(SessionIdentityPublisher::Extension { root_only: false }),
            Self::PrimeAgent => Some(SessionIdentityPublisher::Extension { root_only: true }),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCaptureContext {
    Unsupported,
    PaneScoped,
    Preassigned,
    ManagedExclusiveStore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionCaptureSpec {
    pub backend: SessionCaptureBackend,
    pub host: SessionCaptureContext,
    pub sandbox: SessionCaptureContext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionSupport {
    pub resume: ResumeStrategy,
    pub capture: Option<SessionCaptureSpec>,
}

pub enum ForkStrategy {
    ClaudeFork,
    CodexFork,
    Flag(&'static str),
    Unsupported,
}

/// Data-only lifecycle state. A new variant needs an arm in `AgentDef::lifecycle_label`
/// and in the TS mirrors (`web/src/lib/types.ts`, `web/src/lib/agentProfiles.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AgentLifecycle {
    Active,
    Deprecated {
        since: &'static str,
        note: &'static str,
        replacement: Option<&'static str>,
    },
}

impl AgentLifecycle {
    pub fn is_active(&self) -> bool {
        matches!(self, AgentLifecycle::Active)
    }

    pub fn notice(&self) -> Option<String> {
        if self.is_active() {
            None
        } else {
            Some(self.to_string())
        }
    }
}

impl std::fmt::Display for AgentLifecycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentLifecycle::Active => write!(f, "active"),
            AgentLifecycle::Deprecated {
                since,
                note,
                replacement,
            } => {
                write!(f, "deprecated since {since}: {note}")?;
                match replacement {
                    Some(name) => write!(f, "; consider switching to {name}"),
                    None => Ok(()),
                }
            }
        }
    }
}

/// Hook payload field that names the pane's native conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum HookIdentityField {
    SessionId,
    ConversationIdOrSessionId,
}

#[derive(Debug)]
pub struct HookEvent {
    pub name: &'static str,
    pub matcher: Option<&'static str>,
    pub status: Option<HookStatus>,
    pub identity_field: Option<HookIdentityField>,
    /// Tools that block on the user for their whole run; the hook writes `waiting` for them.
    pub waiting_tools: &'static [&'static str],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedHookEvent {
    pub name: String,
    pub matcher: Option<String>,
    pub status: Option<HookStatus>,
    pub identity_field: Option<HookIdentityField>,
    pub waiting_tools: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SidecarHookEvent {
    pub name: &'static str,
    pub status: HookStatus,
    pub identity_field: Option<HookIdentityField>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookFormat {
    JsonSettings,
    CodexJson,
}

#[derive(Debug)]
pub struct AgentHookConfig {
    pub settings_rel_path: &'static str,
    pub config_dir_env_var: Option<&'static str>,
    pub events: &'static [HookEvent],
    pub format: HookFormat,
}

/// An agent has at most one of `hook_config` or `sidecar_hooks`.
#[derive(Debug)]
pub struct SidecarHooks {
    pub host_config_subpath: &'static str,
    pub sandbox_config_subpath: &'static str,
    pub install: fn(
        &std::path::Path,
        crate::hooks::HookInstallTarget,
        &[ResolvedHookEvent],
    ) -> anyhow::Result<()>,
    pub uninstall: fn(&std::path::Path) -> anyhow::Result<bool>,
    pub post_install_host: Option<fn()>,
    pub selected_agent_hooks: Option<SelectedAgentHooks>,
    pub format: SidecarFormat,
    pub events: &'static [SidecarHookEvent],
}

#[derive(Debug)]
pub struct SelectedAgentHooks {
    pub flag: &'static str,
    pub resolve_config_file: fn(&std::path::Path, &str) -> std::path::PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidecarFormat {
    SettlToml,
    HermesYaml,
    KiroJson,
    KimiToml,
}

pub struct AgentDef {
    pub name: &'static str,
    pub binary: &'static str,
    /// Must not be combined with `ResumeStrategy::Subcommand`, which inserts after the binary.
    pub launch_subcommand: Option<&'static str>,
    pub aliases: &'static [&'static str],
    pub detection: DetectionMethod,
    pub yolo: Option<YoloMode>,
    pub instruction_flag: Option<&'static str>,
    /// One argv token placed before the prompt; never contains a `{}` placeholder.
    pub oneshot_flag: Option<&'static str>,
    pub set_default_command: bool,
    pub detect_status: fn(&str) -> Status,
    pub container_env: &'static [(&'static str, &'static str)],
    pub hook_config: Option<AgentHookConfig>,
    pub sidecar_hooks: Option<SidecarHooks>,
    pub session_support: Option<SessionSupport>,
    pub fork_strategy: ForkStrategy,
    pub host_only: bool,
    /// Delay before Enter, for agents whose paste-burst detection swallows a fast Enter.
    pub send_keys_enter_delay_ms: u64,
    pub ready_marker: Option<&'static str>,
    pub install_hint: &'static str,
    pub permission_response: Option<PermissionResponse>,
    pub lifecycle: AgentLifecycle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyToken {
    Literal(&'static str),
    Named(&'static str),
}

#[derive(Clone, Copy, Debug)]
pub struct PermissionResponse {
    pub allow: &'static [KeyToken],
    pub allow_always: Option<&'static [KeyToken]>,
    pub deny: &'static [KeyToken],
}

const fn hook(name: &'static str, status: HookStatus) -> HookEvent {
    HookEvent {
        name,
        matcher: None,
        status: Some(status),
        identity_field: None,
        waiting_tools: &[],
    }
}

const fn matched_hook(name: &'static str, matcher: &'static str, status: HookStatus) -> HookEvent {
    HookEvent {
        matcher: Some(matcher),
        ..hook(name, status)
    }
}

const fn sidecar(name: &'static str, status: HookStatus) -> SidecarHookEvent {
    SidecarHookEvent {
        name,
        status,
        identity_field: None,
    }
}

/// `Notification`/`idle_prompt` and `StopFailure` backstop `Stop`, which misses API-error and
/// interrupted turn ends. `waiting_tools` marks `AskUserQuestion` as waiting until `PostToolUse`.
const CLAUDE_HOOK_EVENTS: &[HookEvent] = &[
    HookEvent {
        status: None,
        identity_field: Some(HookIdentityField::SessionId),
        ..hook("SessionStart", HookStatus::Idle)
    },
    HookEvent {
        waiting_tools: &["AskUserQuestion"],
        ..hook("PreToolUse", HookStatus::Running)
    },
    matched_hook("PostToolUse", "AskUserQuestion", HookStatus::Running),
    HookEvent {
        identity_field: Some(HookIdentityField::SessionId),
        ..hook("UserPromptSubmit", HookStatus::Running)
    },
    hook("Stop", HookStatus::Idle),
    hook("StopFailure", HookStatus::Idle),
    matched_hook(
        "Notification",
        "permission_prompt|elicitation_dialog|agent_needs_input",
        HookStatus::Waiting,
    ),
    matched_hook(
        "Notification",
        "idle_prompt|agent_completed",
        HookStatus::Idle,
    ),
    hook("ElicitationResult", HookStatus::Running),
];

/// generation_id is turn-scoped and must never be used as resume identity.
const CURSOR_HOOK_EVENTS: &[SidecarHookEvent] = &[
    SidecarHookEvent {
        identity_field: Some(HookIdentityField::ConversationIdOrSessionId),
        ..sidecar("beforeSubmitPrompt", HookStatus::Running)
    },
    sidecar("stop", HookStatus::Idle),
];

const QWEN_HOOK_EVENTS: &[HookEvent] = &[
    hook("PreToolUse", HookStatus::Running),
    hook("UserPromptSubmit", HookStatus::Running),
    hook("PostToolUse", HookStatus::Running),
    hook("Stop", HookStatus::Idle),
    matched_hook(
        "Notification",
        "permission_prompt|elicitation_dialog",
        HookStatus::Waiting,
    ),
];

const CODEX_HOOK_EVENTS: &[HookEvent] = &[
    hook("SessionStart", HookStatus::Idle),
    hook("UserPromptSubmit", HookStatus::Running),
    hook("PreToolUse", HookStatus::Running),
    hook("PermissionRequest", HookStatus::Waiting),
    hook("PostToolUse", HookStatus::Running),
    hook("Stop", HookStatus::Idle),
];

const GEMINI_HOOK_EVENTS: &[HookEvent] = &[
    hook("BeforeTool", HookStatus::Running),
    hook("BeforeAgent", HookStatus::Running),
    hook("AfterAgent", HookStatus::Idle),
    matched_hook("Notification", "ToolPermission", HookStatus::Waiting),
];

pub(crate) const SETTL_SIDECAR_EVENTS: &[SidecarHookEvent] = &[
    sidecar("TurnStarted", HookStatus::Running),
    sidecar("WaitingForHuman", HookStatus::Waiting),
    sidecar("GameWon", HookStatus::Idle),
];

pub(crate) const HERMES_SIDECAR_EVENTS: &[SidecarHookEvent] = &[
    sidecar("pre_llm_call", HookStatus::Running),
    sidecar("pre_tool_call", HookStatus::Running),
    sidecar("post_llm_call", HookStatus::Idle),
    sidecar("pre_approval_request", HookStatus::Waiting),
    sidecar("post_approval_response", HookStatus::Running),
    sidecar("on_session_end", HookStatus::Idle),
];

pub(crate) const KIRO_SIDECAR_EVENTS: &[SidecarHookEvent] = &[
    sidecar("preToolUse", HookStatus::Running),
    sidecar("userPromptSubmit", HookStatus::Running),
    sidecar("stop", HookStatus::Idle),
];

pub(crate) const KIMI_SIDECAR_EVENTS: &[SidecarHookEvent] = &[
    sidecar("UserPromptSubmit", HookStatus::Running),
    sidecar("PreToolUse", HookStatus::Running),
    sidecar("PermissionRequest", HookStatus::Waiting),
    sidecar("PermissionResult", HookStatus::Running),
    sidecar("Stop", HookStatus::Idle),
    sidecar("StopFailure", HookStatus::Idle),
];

const fn session_support(
    resume: ResumeStrategy,
    backend: SessionCaptureBackend,
    host: SessionCaptureContext,
    sandbox: SessionCaptureContext,
) -> Option<SessionSupport> {
    Some(SessionSupport {
        resume,
        capture: Some(SessionCaptureSpec {
            backend,
            host,
            sandbox,
        }),
    })
}

const fn resume_only(resume: ResumeStrategy) -> Option<SessionSupport> {
    Some(SessionSupport {
        resume,
        capture: None,
    })
}

const fn json_hooks(
    settings_rel_path: &'static str,
    events: &'static [HookEvent],
    format: HookFormat,
) -> Option<AgentHookConfig> {
    Some(AgentHookConfig {
        settings_rel_path,
        config_dir_env_var: None,
        events,
        format,
    })
}

/// Defaults shared by every registry entry; each entry overrides what differs.
const fn agent(name: &'static str, binary: &'static str, install_hint: &'static str) -> AgentDef {
    AgentDef {
        name,
        binary,
        launch_subcommand: None,
        aliases: &[],
        detection: DetectionMethod::Which(binary),
        yolo: None,
        instruction_flag: None,
        oneshot_flag: None,
        set_default_command: false,
        detect_status: status_detection::detect_hook_only_status,
        container_env: &[],
        hook_config: None,
        sidecar_hooks: None,
        session_support: None,
        fork_strategy: ForkStrategy::Unsupported,
        host_only: false,
        send_keys_enter_delay_ms: 0,
        ready_marker: None,
        install_hint,
        permission_response: None,
        lifecycle: AgentLifecycle::Active,
    }
}

pub const AGENTS: &[AgentDef] = &[
    AgentDef {
        oneshot_flag: Some("-p"),
        yolo: Some(YoloMode::CliFlag("--dangerously-skip-permissions")),
        instruction_flag: Some("--append-system-prompt {}"),
        detect_status: status_detection::detect_claude_status,
        container_env: &[("CLAUDE_CONFIG_DIR", "/root/.claude")],
        hook_config: Some(AgentHookConfig {
            settings_rel_path: ".claude/settings.json",
            config_dir_env_var: Some("CLAUDE_CONFIG_DIR"),
            events: CLAUDE_HOOK_EVENTS,
            format: HookFormat::JsonSettings,
        }),
        session_support: session_support(
            ResumeStrategy::FlagPair {
                existing: "--resume",
                new_session: "--session-id",
            },
            SessionCaptureBackend::Claude,
            SessionCaptureContext::PaneScoped,
            SessionCaptureContext::PaneScoped,
        ),
        fork_strategy: ForkStrategy::ClaudeFork,
        // Claude's 100ms paste completion window swallows an earlier Enter.
        send_keys_enter_delay_ms: 150,
        permission_response: Some(PermissionResponse {
            allow: &[KeyToken::Literal("1")],
            allow_always: Some(&[KeyToken::Literal("2")]),
            deny: &[KeyToken::Literal("3")],
        }),
        ..agent(
            "claude",
            "claude",
            "npm install -g @anthropic-ai/claude-code",
        )
    },
    AgentDef {
        oneshot_flag: Some("run"),
        aliases: &["open-code"],
        yolo: Some(YoloMode::EnvVar("OPENCODE_PERMISSION", r#"{"*":"allow"}"#)),
        set_default_command: true,
        detect_status: status_detection::detect_opencode_status,
        session_support: session_support(
            ResumeStrategy::Flag("--session"),
            SessionCaptureBackend::OpenCode,
            SessionCaptureContext::Preassigned,
            SessionCaptureContext::Unsupported,
        ),
        fork_strategy: ForkStrategy::Flag("--fork"),
        ready_marker: Some("ask anything"),
        permission_response: Some(PermissionResponse {
            allow: &[KeyToken::Named("Enter")],
            allow_always: Some(&[
                KeyToken::Named("Right"),
                KeyToken::Named("Enter"),
                KeyToken::Named("Enter"),
            ]),
            deny: &[
                KeyToken::Named("Right"),
                KeyToken::Named("Right"),
                KeyToken::Named("Enter"),
            ],
        }),
        ..agent(
            "opencode",
            "opencode",
            "curl -fsSL https://opencode.ai/install | bash",
        )
    },
    AgentDef {
        aliases: &["mistral-vibe"],
        detection: DetectionMethod::RunWithArg("vibe", "--version"),
        yolo: Some(YoloMode::CliFlag("--agent auto-approve")),
        detect_status: status_detection::detect_vibe_status,
        session_support: resume_only(ResumeStrategy::Flag("--resume")),
        ..agent("vibe", "vibe", "pip install mistral-vibe")
    },
    AgentDef {
        oneshot_flag: Some("exec"),
        yolo: Some(YoloMode::CliFlag(
            "--dangerously-bypass-approvals-and-sandbox",
        )),
        instruction_flag: Some("--config developer_instructions={}"),
        set_default_command: true,
        detect_status: status_detection::detect_codex_status,
        hook_config: json_hooks(
            ".codex/hooks.json",
            CODEX_HOOK_EVENTS,
            HookFormat::CodexJson,
        ),
        session_support: session_support(
            ResumeStrategy::Subcommand("resume"),
            SessionCaptureBackend::Codex,
            SessionCaptureContext::Unsupported,
            SessionCaptureContext::ManagedExclusiveStore,
        ),
        fork_strategy: ForkStrategy::CodexFork,
        // Codex's 120ms paste-burst window swallows an earlier Enter.
        send_keys_enter_delay_ms: 150,
        permission_response: Some(PermissionResponse {
            allow: &[KeyToken::Literal("y")],
            allow_always: Some(&[KeyToken::Literal("a")]),
            deny: &[KeyToken::Literal("d")],
        }),
        ..agent("codex", "codex", "npm install -g @openai/codex")
    },
    AgentDef {
        oneshot_flag: Some("-p"),
        yolo: Some(YoloMode::CliFlag("--approval-mode yolo")),
        detect_status: status_detection::detect_gemini_status,
        hook_config: json_hooks(
            ".gemini/settings.json",
            GEMINI_HOOK_EVENTS,
            HookFormat::JsonSettings,
        ),
        session_support: session_support(
            ResumeStrategy::Flag("--resume"),
            SessionCaptureBackend::Gemini,
            SessionCaptureContext::Unsupported,
            SessionCaptureContext::ManagedExclusiveStore,
        ),
        lifecycle: AgentLifecycle::Deprecated {
            since: "2026-06-18",
            note: "consumer accounts cut off by Google; enterprise/API-key remain valid",
            replacement: Some("antigravity"),
        },
        ..agent("gemini", "gemini", "npm install -g @google/gemini-cli")
    },
    AgentDef {
        aliases: &["agent"],
        yolo: Some(YoloMode::CliFlag("--yolo")),
        detect_status: status_detection::detect_cursor_status,
        container_env: &[("CURSOR_CONFIG_DIR", "/root/.cursor")],
        sidecar_hooks: Some(SidecarHooks {
            host_config_subpath: ".cursor/hooks.json",
            sandbox_config_subpath: ".cursor/sandbox/hooks.json",
            install: crate::hooks::install_cursor_hooks_with_events,
            uninstall: crate::hooks::uninstall_cursor_hooks,
            post_install_host: None,
            selected_agent_hooks: None,
            format: SidecarFormat::KiroJson,
            events: CURSOR_HOOK_EVENTS,
        }),
        session_support: session_support(
            ResumeStrategy::Flag("--resume"),
            SessionCaptureBackend::HookSidecar,
            SessionCaptureContext::PaneScoped,
            SessionCaptureContext::PaneScoped,
        ),
        ..agent("cursor", "agent", "see https://docs.cursor.com/cli")
    },
    AgentDef {
        oneshot_flag: Some("-p"),
        aliases: &["github-copilot"],
        yolo: Some(YoloMode::CliFlag("--yolo")),
        detect_status: status_detection::detect_copilot_status,
        container_env: &[("COPILOT_CONFIG_DIR", "/root/.copilot")],
        session_support: resume_only(ResumeStrategy::Flag("--session-id")),
        ..agent(
            "copilot",
            "copilot",
            "see https://docs.github.com/en/copilot/github-copilot-in-the-cli",
        )
    },
    AgentDef {
        yolo: Some(YoloMode::AlwaysYolo),
        detect_status: status_detection::detect_pi_status,
        container_env: &[("PI_CODING_AGENT_DIR", "/root/.pi/agent")],
        // `--session-id` pins a new id; `--session` only resumes an existing one.
        session_support: session_support(
            ResumeStrategy::FlagPair {
                existing: "--session",
                new_session: "--session-id",
            },
            SessionCaptureBackend::Pi,
            SessionCaptureContext::PaneScoped,
            SessionCaptureContext::PaneScoped,
        ),
        ..agent(
            "pi",
            "pi",
            "npm install -g @earendil-works/pi-coding-agent",
        )
    },
    AgentDef {
        aliases: &["factory-droid"],
        yolo: Some(YoloMode::CliFlag("--skip-permissions-unsafe")),
        detect_status: status_detection::detect_droid_status,
        ..agent("droid", "droid", "npm install -g droid")
    },
    AgentDef {
        aliases: &["settlers", "catan"],
        yolo: Some(YoloMode::AlwaysYolo),
        sidecar_hooks: Some(SidecarHooks {
            host_config_subpath: ".settl/config.toml",
            sandbox_config_subpath: "",
            install: crate::hooks::install_settl_hooks_with_events,
            uninstall: crate::hooks::uninstall_settl_hooks,
            post_install_host: None,
            selected_agent_hooks: None,
            format: SidecarFormat::SettlToml,
            events: SETTL_SIDECAR_EVENTS,
        }),
        host_only: true,
        ..agent(
            "settl",
            "settl",
            "brew install --cask mozilla-ai/tap/settl",
        )
    },
    AgentDef {
        yolo: Some(YoloMode::CliFlag("--yolo")),
        detect_status: status_detection::detect_hermes_status,
        container_env: &[("HERMES_ACCEPT_HOOKS", "1")],
        sidecar_hooks: Some(SidecarHooks {
            host_config_subpath: ".hermes/config.yaml",
            sandbox_config_subpath: ".hermes/sandbox/config.yaml",
            install: crate::hooks::install_hermes_hooks_with_events,
            uninstall: crate::hooks::uninstall_hermes_hooks,
            post_install_host: None,
            selected_agent_hooks: None,
            format: SidecarFormat::HermesYaml,
            events: HERMES_SIDECAR_EVENTS,
        }),
        session_support: session_support(
            ResumeStrategy::Flag("--resume"),
            SessionCaptureBackend::Hermes,
            SessionCaptureContext::Unsupported,
            SessionCaptureContext::ManagedExclusiveStore,
        ),
        ..agent(
            "hermes",
            "hermes",
            "curl -fsSL https://raw.githubusercontent.com/NousResearch/hermes-agent/main/scripts/install.sh | bash",
        )
    },
    AgentDef {
        launch_subcommand: Some("chat"),
        aliases: &["kiro-cli"],
        yolo: Some(YoloMode::CliFlag("--trust-all-tools")),
        container_env: &[("KIRO_CONFIG_DIR", "/root/.kiro")],
        sidecar_hooks: Some(SidecarHooks {
            host_config_subpath: crate::hooks::KIRO_HOOKS_AGENT_FILE,
            sandbox_config_subpath: ".kiro/sandbox/agents/aoe-hooks.json",
            install: crate::hooks::install_kiro_hooks_with_events,
            uninstall: crate::hooks::uninstall_kiro_hooks,
            post_install_host: Some(crate::hooks::set_kiro_default_agent_if_builtin),
            selected_agent_hooks: Some(SelectedAgentHooks {
                flag: "--agent",
                resolve_config_file: crate::hooks::resolve_kiro_agent_file,
            }),
            format: SidecarFormat::KiroJson,
            events: KIRO_SIDECAR_EVENTS,
        }),
        ..agent(
            "kiro",
            "kiro-cli",
            "curl -fsSL https://cli.kiro.dev/install | bash",
        )
    },
    AgentDef {
        yolo: Some(YoloMode::CliFlag("--yolo")),
        instruction_flag: Some("--append-system-prompt {}"),
        detect_status: status_detection::detect_qwen_status,
        hook_config: json_hooks(
            ".qwen/settings.json",
            QWEN_HOOK_EVENTS,
            HookFormat::JsonSettings,
        ),
        ..agent("qwen", "qwen", "npm install -g @qwen-code/qwen-code")
    },
    AgentDef {
        aliases: &["agy"],
        yolo: Some(YoloMode::CliFlag("--dangerously-skip-permissions")),
        detect_status: status_detection::detect_antigravity_status,
        ..agent(
            "antigravity",
            "agy",
            "curl -fsSL https://antigravity.google/cli/install.sh | bash",
        )
    },
    AgentDef {
        oneshot_flag: Some("-p"),
        aliases: &["kimi-code"],
        yolo: Some(YoloMode::CliFlag("--yolo")),
        container_env: &[("KIMI_CODE_HOME", "/root/.kimi-code")],
        sidecar_hooks: Some(SidecarHooks {
            host_config_subpath: ".kimi-code/config.toml",
            sandbox_config_subpath: ".kimi-code/sandbox/config.toml",
            install: crate::hooks::install_kimi_hooks_with_events,
            uninstall: crate::hooks::uninstall_kimi_hooks,
            post_install_host: None,
            selected_agent_hooks: None,
            format: SidecarFormat::KimiToml,
            events: KIMI_SIDECAR_EVENTS,
        }),
        session_support: session_support(
            ResumeStrategy::Flag("--session"),
            SessionCaptureBackend::Kimi,
            SessionCaptureContext::Unsupported,
            SessionCaptureContext::ManagedExclusiveStore,
        ),
        ..agent(
            "kimi",
            "kimi",
            "curl -fsSL https://code.kimi.com/kimi-code/install.sh | bash",
        )
    },
    AgentDef {
        oneshot_flag: Some("-p"),
        yolo: Some(YoloMode::CliFlag("--auto-approve")),
        instruction_flag: Some("--append-system-prompt {}"),
        detect_status: status_detection::detect_omp_status,
        container_env: &[("PI_CODING_AGENT_DIR", "/root/.omp/agent")],
        session_support: session_support(
            ResumeStrategy::Flag("--resume"),
            SessionCaptureBackend::Omp,
            SessionCaptureContext::PaneScoped,
            SessionCaptureContext::PaneScoped,
        ),
        permission_response: Some(PermissionResponse {
            allow: &[KeyToken::Named("Enter")],
            allow_always: None,
            deny: &[KeyToken::Named("Down"), KeyToken::Named("Enter")],
        }),
        ..agent("omp", "omp", "curl -fsSL https://omp.sh/install | sh")
    },
    AgentDef {
        oneshot_flag: Some("-p"),
        yolo: Some(YoloMode::AlwaysYolo),
        instruction_flag: Some("--append-system-prompt {}"),
        container_env: &[(
            "PRIME_AGENT_CODING_AGENT_DIR",
            crate::session::config::container_config::PRIME_AGENT_DIR_IN_CONTAINER,
        )],
        session_support: session_support(
            ResumeStrategy::Flag("--resume"),
            SessionCaptureBackend::PrimeAgent,
            SessionCaptureContext::Unsupported,
            SessionCaptureContext::ManagedExclusiveStore,
        ),
        // `--fork` needs the parent id as its value, which `ForkStrategy::Flag` does not emit.
        fork_strategy: ForkStrategy::Unsupported,
        ..agent(
            "prime-agent",
            "prime-agent",
            "curl -fsSL https://app.primeintellect.ai/prime-agent/install.sh | sh",
        )
    },
];

impl AgentDef {
    pub fn lifecycle_label(&self) -> Option<&'static str> {
        match self.lifecycle {
            AgentLifecycle::Active => None,
            AgentLifecycle::Deprecated { .. } => Some("deprecated"),
        }
    }

    pub fn lifecycle_notice(&self) -> Option<String> {
        self.lifecycle.notice()
    }

    /// `codex exec` refuses to run outside a trusted git repo without `--skip-git-repo-check`.
    pub fn oneshot_extra_args(&self) -> &'static [&'static str] {
        match self.name {
            "codex" => &["--skip-git-repo-check"],
            _ => &[],
        }
    }

    pub fn oneshot_trailing_args(&self) -> &'static [&'static str] {
        match self.name {
            "copilot" => &["-s", "--allow-all-tools", "--no-ask-user"],
            _ => &[],
        }
    }

    pub fn oneshot_model_flag(&self) -> Option<&'static str> {
        match self.name {
            "claude" | "copilot" | "omp" | "prime-agent" => Some("--model"),
            "codex" | "gemini" | "opencode" | "kimi" => Some("-m"),
            _ => None,
        }
    }

    /// Stable, non-dated aliases only: AoE pins no CLI version.
    pub fn oneshot_cheap_model(&self) -> Option<&'static str> {
        match self.name {
            "claude" => Some("haiku"),
            _ => None,
        }
    }

    /// Value-binding `-p` (copilot, gemini, kimi) takes the prompt as its value, so model args
    /// must trail the prompt.
    pub fn oneshot_flag_binds_prompt(&self) -> bool {
        matches!(self.name, "copilot" | "gemini" | "kimi")
    }

    pub fn launch_base_command(&self) -> String {
        match self.launch_subcommand {
            Some(sub) => format!("{} {}", self.binary, sub),
            None => self.binary.to_string(),
        }
    }
}

fn help_advertises_flag(help: &str, flag: &str) -> bool {
    help.match_indices(flag).any(|(index, _)| {
        help[index + flag.len()..]
            .chars()
            .next()
            .is_none_or(|next| next.is_whitespace() || next == '=' || next == ',')
    })
}

/// `pi --help`, cached once it succeeds. A timeout or failure reports no flags for now and is
/// retried after a cooldown with a longer deadline, since a cold start with many extensions can
/// outlast the first one.
#[derive(Default)]
struct PiHelpProbe {
    help: Option<String>,
    retry_at: Option<std::time::Instant>,
}

impl PiHelpProbe {
    fn help(
        &mut self,
        clock: impl Fn() -> std::time::Instant,
        probe: impl FnOnce(std::time::Duration) -> Option<String>,
    ) -> &str {
        if self.help.is_none() && self.retry_at.is_none_or(|at| clock() >= at) {
            let timeout = if self.retry_at.is_some() {
                PI_HELP_RETRY_TIMEOUT
            } else {
                PI_HELP_PROBE_TIMEOUT
            };
            self.help = probe(timeout);
            if self.help.is_none() {
                tracing::warn!(target: "session.create", timeout_secs = timeout.as_secs(),
                    "pi --help did not answer; launching without session-id capture until a retry succeeds");
                // Timed from the answer, so a probe that ran to its deadline still cools down.
                self.retry_at = Some(clock() + PI_HELP_RETRY_COOLDOWN);
            }
        }
        self.help.as_deref().unwrap_or_default()
    }
}

fn run_pi_help(timeout: std::time::Duration) -> Option<String> {
    let agent = get_agent("pi")?;
    let mut cmd = std::process::Command::new(agent.binary);
    cmd.arg("--help");
    crate::process::run_with_timeout(&mut cmd, timeout)
        .ok()
        .flatten()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn pi_help_advertises(flag: &str) -> bool {
    static PROBE: std::sync::Mutex<PiHelpProbe> = std::sync::Mutex::new(PiHelpProbe {
        help: None,
        retry_at: None,
    });
    // Held across the probe: launches racing it wait for the answer instead of starting
    // without session-id capture.
    let mut probe = PROBE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    help_advertises_flag(probe.help(std::time::Instant::now, run_pi_help), flag)
}

pub(crate) fn pi_supports_extension_flag() -> bool {
    pi_help_advertises("--extension")
}

pub(crate) fn pi_supports_session_id_flag() -> bool {
    pi_help_advertises("--session-id")
}

const PI_HELP_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const PI_HELP_RETRY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const PI_HELP_RETRY_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(30);

pub fn get_agent(name: &str) -> Option<&'static AgentDef> {
    AGENTS.iter().find(|a| a.name == name)
}

pub(crate) fn registry_lifecycle(name: &str) -> AgentLifecycle {
    get_agent(name)
        .map(|def| def.lifecycle)
        .unwrap_or(AgentLifecycle::Active)
}

/// Only claude qualifies: claude-agent-acp's session id is the id `claude --resume` reads.
pub fn acp_transcript_cli_resumable(tool: &str, acp_agent: &str) -> bool {
    tool == "claude" && matches!(acp_agent, "claude" | "claude-code")
}

fn configured_status_map<'a>(
    config: &'a crate::session::config::Config,
    agent_name: &str,
) -> Option<&'a BTreeMap<String, HookStatus>> {
    config
        .agents
        .get(agent_name)
        .map(|agent| &agent.status_map)
        .filter(|status_map| !status_map.is_empty())
}

// Duplicate events with different matchers collapse to the first default.
fn default_status_map_for_agent(agent: &AgentDef) -> BTreeMap<String, HookStatus> {
    let mut map = BTreeMap::new();
    if let Some(hook_cfg) = &agent.hook_config {
        for event in hook_cfg.events {
            if let Some(status) = event.status {
                map.entry(event.name.to_string()).or_insert(status);
            }
        }
    }
    if let Some(sidecar) = &agent.sidecar_hooks {
        for event in sidecar.events {
            map.entry(event.name.to_string()).or_insert(event.status);
        }
    }
    map
}

pub fn effective_status_map(
    config: &crate::session::config::Config,
    agent_name: &str,
) -> anyhow::Result<BTreeMap<String, HookStatus>> {
    let overrides = configured_status_map(config, agent_name);
    let Some(agent) = get_agent(agent_name) else {
        if let Some(map) = overrides {
            return Ok(map.clone());
        }
        anyhow::bail!(
            "unknown agent '{}' has no configured status_map",
            agent_name
        );
    };

    let mut map = default_status_map_for_agent(agent);
    if let Some(overrides) = overrides {
        for (event, status) in overrides {
            map.insert(event.clone(), *status);
        }
    }

    if map.is_empty() {
        anyhow::bail!("agent '{}' does not declare status hooks", agent.name);
    }
    Ok(map)
}

fn append_configured_status_events(
    events: &mut Vec<ResolvedHookEvent>,
    overrides: Option<&BTreeMap<String, HookStatus>>,
) {
    let Some(overrides) = overrides else {
        return;
    };
    let mut existing: BTreeSet<String> = events.iter().map(|event| event.name.clone()).collect();
    for (name, status) in overrides {
        if existing.insert(name.clone()) {
            events.push(ResolvedHookEvent {
                name: name.clone(),
                matcher: None,
                status: Some(*status),
                identity_field: None,
                waiting_tools: Vec::new(),
            });
        }
    }
}

pub(crate) fn hook_install_required(agent: &AgentDef, status_hooks_enabled: bool) -> bool {
    (status_hooks_enabled && (agent.hook_config.is_some() || agent.sidecar_hooks.is_some()))
        || agent.hook_config.as_ref().is_some_and(|hooks| {
            hooks
                .events
                .iter()
                .any(|event| event.identity_field.is_some())
        })
        || agent.sidecar_hooks.as_ref().is_some_and(|hooks| {
            hooks
                .events
                .iter()
                .any(|event| event.identity_field.is_some())
        })
}

pub fn resolved_hook_events(
    agent: &AgentDef,
    config: &crate::session::config::Config,
) -> anyhow::Result<Vec<ResolvedHookEvent>> {
    let Some(hook_cfg) = &agent.hook_config else {
        return Ok(Vec::new());
    };
    let overrides = configured_status_map(config, agent.name);
    let mut events = hook_cfg
        .events
        .iter()
        .map(|event| ResolvedHookEvent {
            name: event.name.to_string(),
            matcher: event.matcher.map(str::to_string),
            status: overrides
                .and_then(|map| map.get(event.name).copied())
                .or(event.status),
            identity_field: event.identity_field,
            waiting_tools: event.waiting_tools.iter().map(|t| t.to_string()).collect(),
        })
        .collect();
    append_configured_status_events(&mut events, overrides);
    Ok(events)
}

pub fn resolved_sidecar_hook_events(
    agent: &AgentDef,
    config: &crate::session::config::Config,
) -> anyhow::Result<Vec<ResolvedHookEvent>> {
    let Some(sidecar) = &agent.sidecar_hooks else {
        return Ok(Vec::new());
    };
    let overrides = configured_status_map(config, agent.name);
    let mut events = sidecar
        .events
        .iter()
        .map(|event| ResolvedHookEvent {
            name: event.name.to_string(),
            matcher: None,
            status: Some(
                overrides
                    .and_then(|map| map.get(event.name).copied())
                    .unwrap_or(event.status),
            ),
            identity_field: event.identity_field,
            waiting_tools: Vec::new(),
        })
        .collect();
    append_configured_status_events(&mut events, overrides);
    Ok(events)
}

/// The last occurrence wins even when its value is rejected, matching clap-based CLIs.
pub fn parse_selected_agent(args: &str, flag: &str) -> Option<String> {
    let eq_prefix = format!("{flag}=");
    let mut tokens = args.split_whitespace();
    let mut selected = None;
    while let Some(tok) = tokens.next() {
        let value = if let Some(rest) = tok.strip_prefix(&eq_prefix) {
            Some(rest)
        } else if tok == flag {
            tokens.next()
        } else {
            continue;
        };
        selected = value.filter(|&v| is_safe_agent_name(v)).map(str::to_string);
    }
    selected
}

fn is_safe_agent_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.starts_with('-')
        && !name.contains('/')
        && !name.contains('\\')
}

pub fn send_keys_enter_delay(tool: &str) -> u64 {
    get_agent(tool)
        .map(|a| a.send_keys_enter_delay_ms)
        .unwrap_or(0)
}

pub fn ready_marker(tool: &str) -> Option<&'static str> {
    get_agent(tool).and_then(|a| a.ready_marker)
}

pub fn agent_names() -> Vec<&'static str> {
    AGENTS.iter().map(|a| a.name).collect()
}

/// The longest matching token wins: `prime-agent` contains cursor's `agent` alias.
pub fn resolve_tool_name(cmd: &str) -> Option<&'static str> {
    let cmd_lower = cmd.to_lowercase();
    if cmd_lower.is_empty() {
        return Some("claude");
    }
    let mut best: Option<(usize, &'static str)> = None;
    for agent in AGENTS {
        for token in std::iter::once(agent.name).chain(agent.aliases.iter().copied()) {
            if cmd_lower.contains(token) && best.is_none_or(|(len, _)| token.len() > len) {
                best = Some((token.len(), agent.name));
            }
        }
    }
    best.map(|(_, name)| name)
}

pub fn install_hint(name: &str) -> Option<&'static str> {
    get_agent(name).map(|a| a.install_hint)
}

pub fn settings_index_from_name(name: Option<&str>) -> usize {
    match name {
        Some(n) => AGENTS
            .iter()
            .position(|a| a.name == n)
            .map(|i| i + 1)
            .unwrap_or(0),
        None => 0,
    }
}

pub fn name_from_settings_index(index: usize) -> Option<&'static str> {
    if index == 0 {
        None
    } else {
        AGENTS.get(index - 1).map(|a| a.name)
    }
}

pub fn oneshot_capable_names() -> Vec<&'static str> {
    AGENTS
        .iter()
        .filter(|a| a.oneshot_flag.is_some())
        .map(|a| a.name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acp_transcript_cli_resumable_only_for_claude_pairings() {
        assert!(acp_transcript_cli_resumable("claude", "claude"));
        assert!(acp_transcript_cli_resumable("claude", "claude-code"));
        assert!(!acp_transcript_cli_resumable("claude", "codex"));
        assert!(!acp_transcript_cli_resumable("claude", "aoe-agent"));
        assert!(!acp_transcript_cli_resumable("codex", "codex"));
    }

    #[test]
    fn registry_invariants() {
        for agent in AGENTS {
            let name = agent.name;
            assert!(
                !(agent.hook_config.is_some() && agent.sidecar_hooks.is_some()),
                "{name} declares both hook_config and sidecar_hooks"
            );
            if agent.launch_subcommand.is_some() {
                assert!(
                    !matches!(
                        agent.session_support.map(|support| support.resume),
                        Some(ResumeStrategy::Subcommand(_))
                    ),
                    "{name}: a resume subcommand would land before the launch subcommand"
                );
            }

            let mut oneshot_tokens: Vec<&str> = agent.oneshot_flag.into_iter().collect();
            oneshot_tokens.extend(agent.oneshot_model_flag());
            oneshot_tokens.extend(agent.oneshot_cheap_model());
            oneshot_tokens.extend(agent.oneshot_extra_args());
            oneshot_tokens.extend(agent.oneshot_trailing_args());
            for token in oneshot_tokens {
                assert_eq!(token.split_whitespace().count(), 1, "{name}: {token:?}");
                assert_eq!(token, token.trim(), "{name}: {token:?}");
                assert!(!token.contains("{}"), "{name}: {token:?}");
            }

            assert_eq!(
                agent.oneshot_flag.is_some(),
                agent.oneshot_model_flag().is_some(),
                "{name}: a model flag exists exactly for one-shot agents"
            );
            if agent.oneshot_flag_binds_prompt() {
                assert!(agent.oneshot_extra_args().is_empty(), "{name}");
            }
            if agent.oneshot_flag == Some("-p") && !matches!(name, "claude" | "omp" | "prime-agent")
            {
                assert!(agent.oneshot_flag_binds_prompt(), "{name}");
            }
            assert_eq!(agent.oneshot_cheap_model().is_some(), name == "claude");
        }
    }

    #[test]
    fn pi_help_probe_retries_an_inconclusive_answer_and_keeps_a_confirmed_one() {
        let help = "  --session-id <id>\n  --extension, -e <path>\n";
        let start = std::time::Instant::now();
        let now = std::cell::Cell::new(start);
        let mut probe = PiHelpProbe::default();
        let mut timeouts = Vec::new();

        // The first probe runs to its deadline before failing.
        let first = probe.help(
            || now.get(),
            |timeout| {
                timeouts.push(timeout);
                now.set(now.get() + timeout);
                None
            },
        );
        assert_eq!(first, "", "a timed-out probe advertises nothing");
        let failed_at = now.get();
        now.set(failed_at + PI_HELP_RETRY_COOLDOWN - std::time::Duration::from_millis(1));
        assert_eq!(
            probe.help(|| now.get(), |_| unreachable!("cooling down")),
            "",
            "the cooldown runs from the failed answer, not from when the probe began"
        );

        now.set(failed_at + PI_HELP_RETRY_COOLDOWN);
        let retried = probe.help(
            || now.get(),
            |timeout| {
                timeouts.push(timeout);
                Some(help.to_string())
            },
        );
        assert_eq!(retried, help, "the process is not stuck on the failure");
        now.set(now.get() + PI_HELP_RETRY_COOLDOWN);
        assert_eq!(probe.help(|| now.get(), |_| unreachable!("cached")), help);
        assert_eq!(timeouts, [PI_HELP_PROBE_TIMEOUT, PI_HELP_RETRY_TIMEOUT]);
    }

    #[test]
    fn pi_help_probe_matches_only_the_whole_flag() {
        let cases = [
            (
                "  --session-id <id>    Use exact project session ID\n",
                "--session-id",
                true,
            ),
            ("--session-id=<id>", "--session-id", true),
            ("--session-id", "--session-id", true),
            ("  --session-id-file <path>\n", "--session-id", false),
            ("  --extensions-dir <dir>\n", "--extension", false),
            (
                "  --session <path|id>    Use specific session file\n",
                "--session-id",
                false,
            ),
            (
                "  --extension, -e <path>   Load an extension file\n",
                "--extension",
                true,
            ),
        ];
        for (help, flag, expected) in cases {
            assert_eq!(help_advertises_flag(help, flag), expected, "{help:?}");
        }
    }

    #[test]
    fn lifecycle_notice_label_and_wire_shape() {
        assert!(AGENTS
            .iter()
            .all(|a| a.lifecycle.is_active() == (a.name != "gemini")));
        let claude = get_agent("claude").unwrap();
        assert_eq!(claude.lifecycle_label(), None);
        assert_eq!(claude.lifecycle_notice(), None);

        let gemini = get_agent("gemini").unwrap();
        assert_eq!(gemini.lifecycle_label(), Some("deprecated"));
        assert_eq!(
            gemini.lifecycle_notice().as_deref(),
            Some(
                "deprecated since 2026-06-18: consumer accounts cut off by Google; \
                 enterprise/API-key remain valid; consider switching to antigravity"
            )
        );
        let no_replacement = AgentLifecycle::Deprecated {
            since: "2026-01-01",
            note: "upstream shut down",
            replacement: None,
        };
        assert_eq!(
            no_replacement.notice().as_deref(),
            Some("deprecated since 2026-01-01: upstream shut down")
        );

        assert_eq!(
            serde_json::to_string(&AgentLifecycle::Active).unwrap(),
            r#"{"state":"active"}"#
        );
        assert_eq!(
            serde_json::to_string(&gemini.lifecycle).unwrap(),
            concat!(
                r#"{"state":"deprecated","since":"2026-06-18","#,
                r#""note":"consumer accounts cut off by Google; enterprise/API-key remain valid","#,
                r#""replacement":"antigravity"}"#
            )
        );
    }

    #[test]
    fn lifecycle_facts_stay_synced_with_ts_mirror() {
        let mirror_path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web/src/lib/agentProfiles.ts");
        let Ok(mirror) = std::fs::read_to_string(&mirror_path) else {
            // Nix-filtered sources omit web/.
            eprintln!(
                "skipping TS lifecycle mirror check: {} is absent",
                mirror_path.display()
            );
            return;
        };
        let AgentLifecycle::Deprecated {
            since,
            note,
            replacement: Some(replacement),
        } = get_agent("gemini").unwrap().lifecycle
        else {
            panic!("gemini must stay Deprecated with a replacement");
        };
        for fact in [since, note, replacement] {
            assert!(
                mirror.contains(fact),
                "web/src/lib/agentProfiles.ts is missing {fact:?}"
            );
        }
    }

    fn config_with_status(
        agent: &str,
        event: &str,
        status: HookStatus,
    ) -> crate::session::config::Config {
        let mut config = crate::session::config::Config::default();
        config
            .agents
            .entry(agent.to_string())
            .or_default()
            .status_map
            .insert(event.to_string(), status);
        config
    }

    #[test]
    fn status_map_overrides_merge_into_defaults() {
        let claude = get_agent("claude").unwrap();
        let config = config_with_status("claude", "Stop", HookStatus::Error);
        let map = effective_status_map(&config, "claude").unwrap();
        assert_eq!(map.get("PreToolUse"), Some(&HookStatus::Running));
        assert_eq!(map.get("Stop"), Some(&HookStatus::Error));
        assert_eq!(map.get("Notification"), Some(&HookStatus::Waiting));

        let config = config_with_status("claude", "Notification", HookStatus::Running);
        let events = resolved_hook_events(claude, &config).unwrap();
        let notification_statuses: Vec<HookStatus> = events
            .iter()
            .filter(|event| event.name == "Notification")
            .filter_map(|event| event.status)
            .collect();
        assert_eq!(notification_statuses, vec![HookStatus::Running; 2]);

        let config = config_with_status("claude", "PreCompact", HookStatus::Running);
        let map = effective_status_map(&config, "claude").unwrap();
        assert_eq!(map.get("PreCompact"), Some(&HookStatus::Running));
        let events = resolved_hook_events(claude, &config).unwrap();
        let custom = events
            .iter()
            .find(|event| event.name == "PreCompact")
            .unwrap();
        assert_eq!(custom.status, Some(HookStatus::Running));
        assert!(custom.matcher.is_none() && custom.identity_field.is_none());

        let config = config_with_status("vertex", "session.idle", HookStatus::Idle);
        let map = effective_status_map(&config, "vertex").unwrap();
        assert_eq!(map.get("session.idle"), Some(&HookStatus::Idle));
    }

    #[test]
    fn test_resolve_tool_name() {
        let cases = [
            ("claude", Some("claude")),
            ("open-code", Some("opencode")),
            ("mistral-vibe", Some("vibe")),
            ("github-copilot", Some("copilot")),
            ("factory-droid", Some("droid")),
            ("catan", Some("settl")),
            ("kiro-cli", Some("kiro")),
            ("agy", Some("antigravity")),
            ("kimi-code", Some("kimi")),
            ("", Some("claude")),
            ("agent", Some("cursor")),
            // Longest token wins: prime-agent contains cursor's "agent" alias.
            ("prime-agent --mode acp", Some("prime-agent")),
            ("unknown-tool", None),
        ];
        for (cmd, expected) in cases {
            assert_eq!(resolve_tool_name(cmd), expected, "{cmd:?}");
        }
    }

    #[test]
    fn test_settings_index_roundtrip() {
        assert_eq!(settings_index_from_name(None), 0);
        assert_eq!(settings_index_from_name(Some("unknown")), 0);
        assert_eq!(name_from_settings_index(0), None);
        assert_eq!(name_from_settings_index(AGENTS.len() + 1), None);
        for (i, agent) in AGENTS.iter().enumerate() {
            assert_eq!(settings_index_from_name(Some(agent.name)), i + 1);
            assert_eq!(name_from_settings_index(i + 1), Some(agent.name));
        }
    }

    #[test]
    fn test_launch_base_command() {
        assert_eq!(
            get_agent("kiro").unwrap().launch_base_command(),
            "kiro-cli chat"
        );
        assert_eq!(get_agent("claude").unwrap().launch_base_command(), "claude");
    }

    #[test]
    fn test_parse_selected_agent() {
        let cases = [
            ("--agent custom-agent", Some("custom-agent")),
            (
                "--trust-all-tools --agent custom-agent --model x",
                Some("custom-agent"),
            ),
            ("--agent=custom-agent", Some("custom-agent")),
            ("--trust-all-tools", None),
            ("", None),
            ("--foo --agent", None),
            ("--agent --model x", None),
            ("--agent first --agent second", Some("second")),
            ("--agent good --agent ..", None),
            ("--agent good --agent", None),
            ("--agent=", None),
            ("--agent ../../etc/passwd", None),
            ("--agent=a/b", None),
            ("--agent .", None),
        ];
        for (args, expected) in cases {
            assert_eq!(
                parse_selected_agent(args, "--agent").as_deref(),
                expected,
                "{args:?}"
            );
        }
        assert_eq!(
            parse_selected_agent("--profile prod", "--profile").as_deref(),
            Some("prod")
        );
    }

    #[test]
    fn test_send_keys_enter_delay() {
        assert!(send_keys_enter_delay("codex") > 120);
        assert!(send_keys_enter_delay("claude") > 100);
        assert_eq!(send_keys_enter_delay("opencode"), 0);
        assert_eq!(send_keys_enter_delay("unknown_agent"), 0);
    }

    #[test]
    fn pane_hook_capture_agents_declare_their_native_identity_field() {
        for agent in AGENTS {
            let expected = match agent.name {
                "claude" => Some(HookIdentityField::SessionId),
                "cursor" => Some(HookIdentityField::ConversationIdOrSessionId),
                _ => None,
            };
            let hook_fields = agent
                .hook_config
                .iter()
                .flat_map(|config| config.events.iter().filter_map(|e| e.identity_field));
            let sidecar_fields = agent
                .sidecar_hooks
                .iter()
                .flat_map(|config| config.events.iter().filter_map(|e| e.identity_field));
            let fields: Vec<_> = hook_fields.chain(sidecar_fields).collect();
            assert_eq!(!fields.is_empty(), expected.is_some(), "{}", agent.name);
            if let Some(expected) = expected {
                assert!(fields.iter().all(|f| *f == expected), "{}", agent.name);
            }
        }
    }
}
