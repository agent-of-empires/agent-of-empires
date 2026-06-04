//! Centralized agent registry.
//!
//! All per-agent metadata lives here. Adding a new agent means adding one
//! `AgentDef` entry to `AGENTS` and writing a status detection function.

use crate::session::Status;
use crate::tmux::status_detection;

/// How to check whether an agent binary is installed on the host.
pub enum DetectionMethod {
    /// Run `which <binary>` and check exit code.
    Which(&'static str),
    /// Run `<binary> <arg>` and check that it doesn't error (e.g. `vibe --version`).
    RunWithArg(&'static str, &'static str),
}

/// How to enable YOLO / auto-approve mode for an agent.
pub enum YoloMode {
    /// Append a CLI flag (e.g. `--dangerously-skip-permissions`).
    CliFlag(&'static str),
    /// Set an environment variable (name, value).
    EnvVar(&'static str, &'static str),
    /// Agent always runs in YOLO mode with no opt-in needed (e.g. pi).
    AlwaysYolo,
}

/// How an agent resumes an existing session from the CLI.
pub enum ResumeStrategy {
    /// Append a flag (e.g. `--session <id>`). For agents where new and existing
    /// sessions use the same flag.
    Flag(&'static str),
    /// Two different flags depending on whether conversation data already exists.
    /// `existing` is used when there is prior conversation data (e.g. `--resume`),
    /// `new_session` when creating/attaching unconditionally (e.g. `--session-id`).
    FlagPair {
        existing: &'static str,
        new_session: &'static str,
    },
    /// Resume is a subcommand rather than a flag (e.g. `codex resume <id>`).
    /// The subcommand + id are inserted right after the binary name so that
    /// other flags land after it.
    Subcommand(&'static str),
    /// Agent does not support session resume.
    Unsupported,
}

/// A single hook event that AoE registers in an agent's settings file.
pub struct HookEvent {
    /// Event name as the agent expects it (e.g. `"PreToolUse"` for Claude Code).
    pub name: &'static str,
    /// Optional matcher pattern (e.g. `"permission_prompt|elicitation_dialog"`).
    pub matcher: Option<&'static str>,
    /// AoE status to write when this event fires (`"running"`, `"idle"`, `"waiting"`).
    pub status: Option<&'static str>,
    /// When `true`, install an additional hook command that extracts
    /// `session_id` from the agent's stdin JSON payload and writes it to
    /// `/tmp/aoe-hooks/<AOE_INSTANCE_ID>/session_id`.
    pub session_id_capture: bool,
}

/// Configuration for installing status-detection hooks into an agent's settings file.
pub struct AgentHookConfig {
    /// Path relative to the home dir where the agent's settings live
    /// (e.g. `.claude/settings.json`).
    pub settings_rel_path: &'static str,
    /// Hook events to register (status transitions and session lifecycle).
    pub events: &'static [HookEvent],
}

/// Everything we know about a single agent CLI.
pub struct AgentDef {
    /// Canonical name: `"claude"`, `"opencode"`, etc.
    pub name: &'static str,
    /// Binary to invoke (usually same as name).
    pub binary: &'static str,
    /// Alternative substrings recognised by `resolve_tool_name` (e.g. `"open-code"`).
    pub aliases: &'static [&'static str],
    /// How to detect availability on the host.
    pub detection: DetectionMethod,
    /// YOLO/auto-approve configuration.
    pub yolo: Option<YoloMode>,
    /// CLI flag template for custom instruction injection.
    /// `{}` is replaced with the shell-escaped instruction text.
    pub instruction_flag: Option<&'static str>,
    /// If true, `builder.rs` sets `instance.command = binary` for this agent.
    pub set_default_command: bool,
    /// Status detection function pointer. Takes raw (non-lowercased) pane content.
    pub detect_status: fn(&str) -> Status,
    /// Environment variables always injected into the container for this agent.
    pub container_env: &'static [(&'static str, &'static str)],
    /// Hook configuration for file-based status detection. If set, AoE installs
    /// hooks into the agent's settings file so status is written to a file instead
    /// of being parsed from tmux pane content.
    pub hook_config: Option<AgentHookConfig>,
    /// How this agent resumes a prior session.
    pub resume_strategy: ResumeStrategy,
    /// If true, this agent can only run on the host (no sandbox/worktree support).
    /// The new-session dialog hides sandbox and worktree options for these agents.
    pub host_only: bool,
    /// Milliseconds to wait between sending literal text and the final Enter key.
    /// Agents with paste-burst detection (e.g. Codex, 120ms window) swallow Enter
    /// keys that arrive too quickly after a stream of characters, treating them as
    /// newlines within a paste rather than as "submit". A delay longer than the
    /// agent's burst window lets the suppression expire before Enter arrives.
    pub send_keys_enter_delay_ms: u64,
    /// One-line install command shown when the agent is missing from PATH.
    pub install_hint: &'static str,
}

/// Claude Code hook events. `SessionStart` and `UserPromptSubmit` carry
/// `session_id_capture: true` so the per-instance sidecar
/// (`/tmp/aoe-hooks/<id>/session_id`) is updated whenever Claude rotates
/// its session UUID (`/clear`, `/new`, `--fork-session`, resume, compact).
/// `claude_poll_fn` reads this sidecar before falling back to its disk
/// scan.
const CLAUDE_HOOK_EVENTS: &[HookEvent] = &[
    HookEvent {
        name: "SessionStart",
        matcher: None,
        status: None,
        session_id_capture: true,
    },
    HookEvent {
        name: "PreToolUse",
        matcher: None,
        status: Some("running"),
        session_id_capture: false,
    },
    HookEvent {
        name: "UserPromptSubmit",
        matcher: None,
        status: Some("running"),
        session_id_capture: true,
    },
    HookEvent {
        name: "Stop",
        matcher: None,
        status: Some("idle"),
        session_id_capture: false,
    },
    HookEvent {
        name: "Notification",
        matcher: Some("permission_prompt|elicitation_dialog"),
        status: Some("waiting"),
        session_id_capture: false,
    },
    HookEvent {
        name: "ElicitationResult",
        matcher: None,
        status: Some("running"),
        session_id_capture: false,
    },
];

/// Cursor CLI hook events. No `session_id_capture`: Cursor's session id is
/// not consumed by AoE pollers, and Cursor's hook payload uses a different
/// schema, so installing the capture command would do useless work on every
/// `UserPromptSubmit`.
const CURSOR_HOOK_EVENTS: &[HookEvent] = &[
    HookEvent {
        name: "PreToolUse",
        matcher: None,
        status: Some("running"),
        session_id_capture: false,
    },
    HookEvent {
        name: "UserPromptSubmit",
        matcher: None,
        status: Some("running"),
        session_id_capture: false,
    },
    HookEvent {
        name: "Stop",
        matcher: None,
        status: Some("idle"),
        session_id_capture: false,
    },
    HookEvent {
        name: "Notification",
        matcher: Some("permission_prompt|elicitation_dialog"),
        status: Some("waiting"),
        session_id_capture: false,
    },
    HookEvent {
        name: "ElicitationResult",
        matcher: None,
        status: Some("running"),
        session_id_capture: false,
    },
];

/// Qwen Code uses the same Claude-style event schema and `permission_prompt`/
/// `elicitation_dialog` notification types, but does not emit `ElicitationResult`.
/// `PostToolUse` is used instead to clear the waiting state after the user
/// approves a permission prompt and the tool runs to completion.
const QWEN_HOOK_EVENTS: &[HookEvent] = &[
    HookEvent {
        name: "PreToolUse",
        matcher: None,
        status: Some("running"),
        session_id_capture: false,
    },
    HookEvent {
        name: "UserPromptSubmit",
        matcher: None,
        status: Some("running"),
        session_id_capture: false,
    },
    HookEvent {
        name: "PostToolUse",
        matcher: None,
        status: Some("running"),
        session_id_capture: false,
    },
    HookEvent {
        name: "Stop",
        matcher: None,
        status: Some("idle"),
        session_id_capture: false,
    },
    HookEvent {
        name: "Notification",
        matcher: Some("permission_prompt|elicitation_dialog"),
        status: Some("waiting"),
        session_id_capture: false,
    },
];

/// Codex hook events. Codex loads these from the `[hooks]` table in
/// `~/.codex/config.toml`.
const CODEX_HOOK_EVENTS: &[HookEvent] = &[
    HookEvent {
        name: "SessionStart",
        matcher: None,
        status: Some("idle"),
        session_id_capture: false,
    },
    HookEvent {
        name: "UserPromptSubmit",
        matcher: None,
        status: Some("running"),
        session_id_capture: false,
    },
    HookEvent {
        name: "PreToolUse",
        matcher: None,
        status: Some("running"),
        session_id_capture: false,
    },
    HookEvent {
        name: "PermissionRequest",
        matcher: None,
        status: Some("waiting"),
        session_id_capture: false,
    },
    HookEvent {
        name: "PostToolUse",
        matcher: None,
        status: Some("running"),
        session_id_capture: false,
    },
    HookEvent {
        name: "Stop",
        matcher: None,
        status: Some("idle"),
        session_id_capture: false,
    },
];

pub const AGENTS: &[AgentDef] = &[
    AgentDef {
        name: "claude",
        binary: "claude",
        aliases: &[],
        detection: DetectionMethod::Which("claude"),
        yolo: Some(YoloMode::CliFlag("--dangerously-skip-permissions")),
        instruction_flag: Some("--append-system-prompt {}"),
        set_default_command: false,
        detect_status: status_detection::detect_claude_status,
        container_env: &[("CLAUDE_CONFIG_DIR", "/root/.claude")],
        hook_config: Some(AgentHookConfig {
            settings_rel_path: ".claude/settings.json",
            events: CLAUDE_HOOK_EVENTS,
        }),
        resume_strategy: ResumeStrategy::FlagPair {
            existing: "--resume",
            new_session: "--session-id",
        },
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "npm install -g @anthropic-ai/claude-code",
    },
    AgentDef {
        name: "opencode",
        binary: "opencode",
        aliases: &["open-code"],
        detection: DetectionMethod::Which("opencode"),
        yolo: Some(YoloMode::EnvVar("OPENCODE_PERMISSION", r#"{"*":"allow"}"#)),
        instruction_flag: None,
        set_default_command: true,
        detect_status: status_detection::detect_opencode_status,
        container_env: &[],
        hook_config: None,
        resume_strategy: ResumeStrategy::Flag("--session"),
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "curl -fsSL https://opencode.ai/install | bash",
    },
    AgentDef {
        name: "vibe",
        binary: "vibe",
        aliases: &["mistral-vibe"],
        detection: DetectionMethod::RunWithArg("vibe", "--version"),
        yolo: Some(YoloMode::CliFlag("--agent auto-approve")),
        instruction_flag: None,
        set_default_command: false,
        detect_status: status_detection::detect_vibe_status,
        container_env: &[],
        hook_config: None,
        resume_strategy: ResumeStrategy::Flag("--resume"),
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "pip install mistral-vibe",
    },
    AgentDef {
        name: "codex",
        binary: "codex",
        aliases: &[],
        detection: DetectionMethod::Which("codex"),
        yolo: Some(YoloMode::CliFlag(
            "--dangerously-bypass-approvals-and-sandbox",
        )),
        instruction_flag: Some("--config developer_instructions={}"),
        set_default_command: true,
        detect_status: status_detection::detect_codex_status,
        container_env: &[],
        hook_config: Some(AgentHookConfig {
            settings_rel_path: ".codex/config.toml",
            events: CODEX_HOOK_EVENTS,
        }),
        resume_strategy: ResumeStrategy::Subcommand("resume"),
        host_only: false,
        // Codex has paste-burst detection with a 120ms Enter-suppression window;
        // Enter keys arriving within that window after a character stream are
        // swallowed as newlines instead of triggering submit. 150ms > 120ms.
        send_keys_enter_delay_ms: 150,
        install_hint: "npm install -g @openai/codex",
    },
    AgentDef {
        name: "gemini",
        binary: "gemini",
        aliases: &[],
        detection: DetectionMethod::Which("gemini"),
        yolo: Some(YoloMode::CliFlag("--approval-mode yolo")),
        instruction_flag: None,
        set_default_command: false,
        detect_status: status_detection::detect_gemini_status,
        container_env: &[],
        hook_config: Some(AgentHookConfig {
            settings_rel_path: ".gemini/settings.json",
            events: &[
                HookEvent {
                    name: "BeforeTool",
                    matcher: None,
                    status: Some("running"),
                    session_id_capture: false,
                },
                HookEvent {
                    name: "BeforeAgent",
                    matcher: None,
                    status: Some("running"),
                    session_id_capture: false,
                },
                HookEvent {
                    name: "AfterAgent",
                    matcher: None,
                    status: Some("idle"),
                    session_id_capture: false,
                },
                HookEvent {
                    name: "Notification",
                    matcher: Some("ToolPermission"),
                    status: Some("waiting"),
                    session_id_capture: false,
                },
            ],
        }),
        resume_strategy: ResumeStrategy::Flag("--resume"),
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "npm install -g @google/gemini-cli",
    },
    AgentDef {
        name: "cursor",
        binary: "agent",
        aliases: &["agent"],
        detection: DetectionMethod::Which("agent"),
        yolo: Some(YoloMode::CliFlag("--yolo")),
        instruction_flag: None,
        set_default_command: false,
        detect_status: status_detection::detect_cursor_status,
        container_env: &[("CURSOR_CONFIG_DIR", "/root/.cursor")],
        hook_config: Some(AgentHookConfig {
            settings_rel_path: ".cursor/settings.json",
            events: CURSOR_HOOK_EVENTS,
        }),
        resume_strategy: ResumeStrategy::Unsupported,
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "see https://docs.cursor.com/cli",
    },
    AgentDef {
        name: "copilot",
        binary: "copilot",
        aliases: &["github-copilot"],
        detection: DetectionMethod::Which("copilot"),
        yolo: Some(YoloMode::CliFlag("--yolo")),
        instruction_flag: None,
        set_default_command: false,
        detect_status: status_detection::detect_copilot_status,
        container_env: &[("COPILOT_CONFIG_DIR", "/root/.copilot")],
        hook_config: None,
        resume_strategy: ResumeStrategy::Unsupported,
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "see https://docs.github.com/en/copilot/github-copilot-in-the-cli",
    },
    AgentDef {
        name: "pi",
        binary: "pi",
        aliases: &[],
        detection: DetectionMethod::Which("pi"),
        // Pi runs in full YOLO mode by default (no approval gates), so no flag needed.
        yolo: Some(YoloMode::AlwaysYolo),
        instruction_flag: None,
        set_default_command: false,
        detect_status: status_detection::detect_pi_status,
        container_env: &[("PI_CODING_AGENT_DIR", "/root/.pi/agent")],
        hook_config: None,
        resume_strategy: ResumeStrategy::Flag("--session"),
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "npm install -g @earendil-works/pi-coding-agent",
    },
    AgentDef {
        name: "droid",
        binary: "droid",
        aliases: &["factory-droid"],
        detection: DetectionMethod::Which("droid"),
        yolo: Some(YoloMode::CliFlag("--skip-permissions-unsafe")),
        instruction_flag: None,
        set_default_command: false,
        detect_status: status_detection::detect_droid_status,
        container_env: &[],
        hook_config: None,
        resume_strategy: ResumeStrategy::Unsupported,
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "npm install -g droid",
    },
    AgentDef {
        name: "settl",
        binary: "settl",
        aliases: &["settlers", "catan"],
        detection: DetectionMethod::Which("settl"),
        yolo: Some(YoloMode::AlwaysYolo),
        instruction_flag: None,
        set_default_command: false,
        detect_status: status_detection::detect_settl_status,
        container_env: &[],
        hook_config: None,
        resume_strategy: ResumeStrategy::Unsupported,
        host_only: true,
        send_keys_enter_delay_ms: 0,
        install_hint: "brew install --cask mozilla-ai/tap/settl",
    },
    AgentDef {
        name: "hermes",
        binary: "hermes",
        aliases: &[],
        detection: DetectionMethod::Which("hermes"),
        yolo: Some(YoloMode::CliFlag("--yolo")),
        instruction_flag: None,
        set_default_command: false,
        // Status is detected via Hermes's shell-hook system (YAML config),
        // installed by hooks::install_hermes_hooks(); the stub here just
        // returns Idle as a fallback before the first hook fires.
        detect_status: status_detection::detect_hermes_status,
        // HERMES_ACCEPT_HOOKS bypasses the first-use TTY consent prompt for
        // shell hooks. Hermes still gates each (event, command) on its
        // allowlist file, which AoE pre-populates in install_hermes_hooks.
        container_env: &[("HERMES_ACCEPT_HOOKS", "1")],
        // Hermes uses YAML (`hooks: { event: [...] }`) rather than the
        // JSON settings.json schema shared by Claude/Cursor/Gemini, so
        // hook_config: None and install is special-cased like settl.
        hook_config: None,
        resume_strategy: ResumeStrategy::Flag("--resume"),
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint:
            "curl -fsSL https://raw.githubusercontent.com/NousResearch/hermes-agent/main/scripts/install.sh | bash",
    },
    AgentDef {
        name: "kiro",
        binary: "kiro-cli",
        aliases: &["kiro-cli"],
        detection: DetectionMethod::Which("kiro-cli"),
        yolo: Some(YoloMode::CliFlag("--trust-all-tools")),
        instruction_flag: None,
        set_default_command: false,
        detect_status: status_detection::detect_kiro_status,
        container_env: &[("KIRO_CONFIG_DIR", "/root/.kiro")],
        // Kiro uses a per-agent JSON config (lowercase event names, flat
        // {command} objects) rather than the JSON settings.json schema shared
        // by Claude/Cursor/Gemini, so hook_config: None and install is
        // special-cased like hermes/settl. Status comes from the hook sidecar
        // file written by install_kiro_hooks; the pane stub is unused.
        hook_config: None,
        resume_strategy: ResumeStrategy::Flag("--resume-id"),
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "curl -fsSL https://cli.kiro.dev/install | bash",
    },
    AgentDef {
        name: "qwen",
        binary: "qwen",
        aliases: &[],
        detection: DetectionMethod::Which("qwen"),
        yolo: Some(YoloMode::CliFlag("--yolo")),
        instruction_flag: Some("--append-system-prompt {}"),
        set_default_command: false,
        detect_status: status_detection::detect_qwen_status,
        container_env: &[],
        hook_config: Some(AgentHookConfig {
            settings_rel_path: ".qwen/settings.json",
            events: QWEN_HOOK_EVENTS,
        }),
        resume_strategy: ResumeStrategy::FlagPair {
            existing: "--resume",
            new_session: "--session-id",
        },
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "npm install -g @qwen-code/qwen-code",
    },
    AgentDef {
        name: "antigravity",
        binary: "agy",
        aliases: &["agy"],
        detection: DetectionMethod::Which("agy"),
        yolo: Some(YoloMode::CliFlag("--dangerously-skip-permissions")),
        instruction_flag: None,
        set_default_command: false,
        detect_status: status_detection::detect_antigravity_status,
        container_env: &[],
        hook_config: None,
        resume_strategy: ResumeStrategy::Unsupported,
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "curl -fsSL https://antigravity.google/cli/install.sh | bash",
    },
];

/// Look up an agent by canonical name.
pub fn get_agent(name: &str) -> Option<&'static AgentDef> {
    AGENTS.iter().find(|a| a.name == name)
}

/// Returns the delay (in ms) to insert before the submit-Enter for this agent.
/// Non-zero for agents with paste-burst detection that swallows fast Enters.
pub fn send_keys_enter_delay(tool: &str) -> u64 {
    get_agent(tool)
        .map(|a| a.send_keys_enter_delay_ms)
        .unwrap_or(0)
}

/// All canonical agent names in registry order.
pub fn agent_names() -> Vec<&'static str> {
    AGENTS.iter().map(|a| a.name).collect()
}

/// Given a command string (e.g. `"claude --resume xyz"` or `"open-code"`),
/// return the canonical agent name if one is recognised.
pub fn resolve_tool_name(cmd: &str) -> Option<&'static str> {
    let cmd_lower = cmd.to_lowercase();
    if cmd_lower.is_empty() {
        return Some("claude");
    }
    for agent in AGENTS {
        if cmd_lower.contains(agent.name) {
            return Some(agent.name);
        }
        for alias in agent.aliases {
            if cmd_lower.contains(alias) {
                return Some(agent.name);
            }
        }
    }
    None
}

/// Return the install hint for an agent, looked up by canonical name.
pub fn install_hint(name: &str) -> Option<&'static str> {
    get_agent(name).map(|a| a.install_hint)
}

/// Convert a tool name to a 1-based settings index (0 = Auto).
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

/// Convert a 1-based settings index back to a tool name (0 = Auto/None).
pub fn name_from_settings_index(index: usize) -> Option<&'static str> {
    if index == 0 {
        None
    } else {
        AGENTS.get(index - 1).map(|a| a.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_agent_known() {
        assert_eq!(get_agent("claude").unwrap().binary, "claude");
        assert_eq!(get_agent("opencode").unwrap().binary, "opencode");
        assert_eq!(get_agent("vibe").unwrap().binary, "vibe");
        assert_eq!(get_agent("codex").unwrap().binary, "codex");
        assert_eq!(get_agent("gemini").unwrap().binary, "gemini");
        assert_eq!(get_agent("cursor").unwrap().binary, "agent");
        assert_eq!(get_agent("copilot").unwrap().binary, "copilot");
        assert_eq!(get_agent("pi").unwrap().binary, "pi");
        assert_eq!(get_agent("droid").unwrap().binary, "droid");
        assert_eq!(get_agent("settl").unwrap().binary, "settl");
        assert_eq!(get_agent("hermes").unwrap().binary, "hermes");
        assert_eq!(get_agent("kiro").unwrap().binary, "kiro-cli");
        assert_eq!(get_agent("qwen").unwrap().binary, "qwen");
        assert_eq!(get_agent("antigravity").unwrap().binary, "agy");
    }

    #[test]
    fn test_hermes_agent_definition() {
        let hermes = get_agent("hermes").unwrap();
        assert_eq!(hermes.binary, "hermes");
        assert!(matches!(
            &hermes.detection,
            DetectionMethod::Which("hermes")
        ));
        assert!(matches!(&hermes.yolo, Some(YoloMode::CliFlag("--yolo"))));
        assert!(!hermes.host_only);
        assert_eq!(hermes.send_keys_enter_delay_ms, 0);
        assert_eq!(
            hermes.install_hint,
            "curl -fsSL https://raw.githubusercontent.com/NousResearch/hermes-agent/main/scripts/install.sh | bash"
        );
    }

    #[test]
    fn test_get_agent_unknown() {
        assert!(get_agent("unknown").is_none());
    }

    #[test]
    fn test_agent_names() {
        let names = agent_names();
        assert_eq!(
            names,
            vec![
                "claude",
                "opencode",
                "vibe",
                "codex",
                "gemini",
                "cursor",
                "copilot",
                "pi",
                "droid",
                "settl",
                "hermes",
                "kiro",
                "qwen",
                "antigravity"
            ]
        );
    }

    #[test]
    fn test_resolve_tool_name() {
        assert_eq!(resolve_tool_name("claude"), Some("claude"));
        assert_eq!(resolve_tool_name("open-code"), Some("opencode"));
        assert_eq!(resolve_tool_name("mistral-vibe"), Some("vibe"));
        assert_eq!(resolve_tool_name("codex"), Some("codex"));
        assert_eq!(resolve_tool_name("gemini"), Some("gemini"));
        assert_eq!(resolve_tool_name("cursor"), Some("cursor"));
        assert_eq!(resolve_tool_name("github-copilot"), Some("copilot"));
        assert_eq!(resolve_tool_name("copilot"), Some("copilot"));
        assert_eq!(resolve_tool_name("pi"), Some("pi"));
        assert_eq!(resolve_tool_name("droid"), Some("droid"));
        assert_eq!(resolve_tool_name("factory-droid"), Some("droid"));
        assert_eq!(resolve_tool_name("settl"), Some("settl"));
        assert_eq!(resolve_tool_name("settlers"), Some("settl"));
        assert_eq!(resolve_tool_name("catan"), Some("settl"));
        assert_eq!(resolve_tool_name("hermes"), Some("hermes"));
        assert_eq!(resolve_tool_name("kiro"), Some("kiro"));
        assert_eq!(resolve_tool_name("kiro-cli"), Some("kiro"));
        assert_eq!(resolve_tool_name("qwen"), Some("qwen"));
        assert_eq!(resolve_tool_name("antigravity"), Some("antigravity"));
        assert_eq!(resolve_tool_name("agy"), Some("antigravity"));
        assert_eq!(resolve_tool_name(""), Some("claude"));
        assert_eq!(resolve_tool_name("agent"), Some("cursor"));
        assert_eq!(resolve_tool_name("unknown-tool"), None);
    }

    #[test]
    fn test_settings_index_roundtrip() {
        assert_eq!(settings_index_from_name(None), 0);
        assert_eq!(settings_index_from_name(Some("claude")), 1);
        assert_eq!(settings_index_from_name(Some("gemini")), 5);
        assert_eq!(settings_index_from_name(Some("cursor")), 6);
        assert_eq!(settings_index_from_name(Some("copilot")), 7);
        assert_eq!(settings_index_from_name(Some("pi")), 8);
        assert_eq!(settings_index_from_name(Some("droid")), 9);
        assert_eq!(settings_index_from_name(Some("settl")), 10);
        assert_eq!(settings_index_from_name(Some("hermes")), 11);
        assert_eq!(settings_index_from_name(Some("kiro")), 12);
        assert_eq!(settings_index_from_name(Some("qwen")), 13);
        assert_eq!(settings_index_from_name(Some("antigravity")), 14);

        assert_eq!(name_from_settings_index(0), None);
        assert_eq!(name_from_settings_index(1), Some("claude"));
        assert_eq!(name_from_settings_index(5), Some("gemini"));
        assert_eq!(name_from_settings_index(6), Some("cursor"));
        assert_eq!(name_from_settings_index(7), Some("copilot"));
        assert_eq!(name_from_settings_index(8), Some("pi"));
        assert_eq!(name_from_settings_index(9), Some("droid"));
        assert_eq!(name_from_settings_index(10), Some("settl"));
        assert_eq!(name_from_settings_index(11), Some("hermes"));
        assert_eq!(name_from_settings_index(12), Some("kiro"));
        assert_eq!(name_from_settings_index(13), Some("qwen"));
        assert_eq!(name_from_settings_index(14), Some("antigravity"));
        assert_eq!(name_from_settings_index(99), None);
    }

    #[test]
    fn test_all_agents_have_yolo_support() {
        for agent in AGENTS {
            assert!(
                agent.yolo.is_some(),
                "Agent '{}' should have YOLO mode configured",
                agent.name
            );
        }
    }

    #[test]
    fn test_send_keys_enter_delay() {
        // Codex needs a delay to outlast its 120ms paste-burst suppression window
        assert!(send_keys_enter_delay("codex") >= 150);
        // Other agents should not delay
        assert_eq!(send_keys_enter_delay("claude"), 0);
        assert_eq!(send_keys_enter_delay("opencode"), 0);
        assert_eq!(send_keys_enter_delay("hermes"), 0);
        assert_eq!(send_keys_enter_delay("kiro"), 0);
        assert_eq!(send_keys_enter_delay("antigravity"), 0);
        assert_eq!(send_keys_enter_delay("unknown_agent"), 0);
    }

    #[test]
    fn test_all_agents_have_install_hint() {
        for agent in AGENTS {
            assert!(
                !agent.install_hint.is_empty(),
                "Agent '{}' should have a non-empty install_hint",
                agent.name
            );
        }
    }

    #[test]
    fn test_install_hint_lookup() {
        assert_eq!(
            install_hint("claude"),
            Some("npm install -g @anthropic-ai/claude-code")
        );
        assert_eq!(install_hint("codex"), Some("npm install -g @openai/codex"));
        // Pi is distributed via npm, not pip (issue #818).
        assert_eq!(
            install_hint("pi"),
            Some("npm install -g @earendil-works/pi-coding-agent")
        );
        // Mistral Vibe's PyPI package is `mistral-vibe`, not `vibe-tool`.
        assert_eq!(install_hint("vibe"), Some("pip install mistral-vibe"));
        // Factory's Droid CLI npm package is `droid`; `@anthropic-ai/droid`
        // does not exist on the registry.
        assert_eq!(install_hint("droid"), Some("npm install -g droid"));
        // settl ships via the mozilla-ai Homebrew tap (settl.dev is unrelated).
        assert_eq!(
            install_hint("settl"),
            Some("brew install --cask mozilla-ai/tap/settl")
        );
        assert_eq!(
            install_hint("hermes"),
            Some("curl -fsSL https://raw.githubusercontent.com/NousResearch/hermes-agent/main/scripts/install.sh | bash")
        );
        assert_eq!(
            install_hint("kiro"),
            Some("curl -fsSL https://cli.kiro.dev/install | bash")
        );
        assert_eq!(
            install_hint("antigravity"),
            Some("curl -fsSL https://antigravity.google/cli/install.sh | bash")
        );
        assert!(install_hint("unknown").is_none());
    }
}
