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
#[derive(Debug)]
pub struct HookEvent {
    /// Event name as the agent expects it (e.g. `"PreToolUse"` for Claude Code).
    pub name: &'static str,
    /// Optional matcher pattern (e.g. `"permission_prompt|elicitation_dialog"`).
    pub matcher: Option<&'static str>,
    /// AoE status to write when this event fires (`"running"`, `"idle"`, `"waiting"`).
    pub status: Option<&'static str>,
    /// When `true`, install an additional hook command that extracts
    /// `session_id` from the agent's stdin JSON payload and writes it to
    /// `/tmp/aoe-hooks-<euid>/<AOE_INSTANCE_ID>/session_id`.
    pub session_id_capture: bool,
}

/// On-disk format an agent uses for its status-detection hooks. Each variant
/// drives one install path: `JsonSettings` goes through the generic
/// `hooks.<event>[].hooks[].command` JSON writer used by Claude-shape agents;
/// `CodexJson` shares the same JSON payload but resolves its path through
/// Codex's `CODEX_HOME` convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookFormat {
    /// JSON `settings.json` with `hooks.<event>[].hooks[].command`. Used by
    /// Claude, Cursor, Gemini, Qwen, and any future agent that adopts this
    /// shape.
    JsonSettings,
    /// Codex `hooks.json`. Identical JSON payload shape to `JsonSettings`,
    /// but the path is resolved via `CODEX_HOME` → `~/.codex/hooks.json`.
    /// Codex's `[hooks.state]` trust block lives in `config.toml` and is
    /// untouched by this writer.
    CodexJson,
}

/// Configuration for installing status-detection hooks into an agent's settings file.
#[derive(Debug)]
pub struct AgentHookConfig {
    /// Path relative to the home dir where the agent's settings live
    /// (e.g. `.claude/settings.json`).
    pub settings_rel_path: &'static str,
    /// Optional env var that overrides the agent's config directory
    /// (e.g. `CLAUDE_CONFIG_DIR`). When set in the session's host environment,
    /// or in AoE's own environment, the settings file lives directly under that
    /// directory using the basename of `settings_rel_path`, rather than under
    /// `~/<settings_rel_path>`. `None` for agents with a fixed home-relative path.
    pub config_dir_env_var: Option<&'static str>,
    /// Hook events to register (status transitions and session lifecycle).
    pub events: &'static [HookEvent],
    /// On-disk format of the settings file. Drives target-kind selection in
    /// [`crate::hooks::iter_hook_targets_in`], which feeds the v015 marker
    /// walker and the uninstall path.
    pub format: HookFormat,
}

/// Installer for an agent whose status hooks live in a config format the
/// generic [`AgentHookConfig`] (JSON `settings.json`) path cannot emit: settl
/// (TOML), hermes (YAML), kiro (per-agent JSON). Bundling the host path, the
/// sandbox path, and the install/uninstall function pointers here lets every
/// call site (`status_hook_env_prefix`, host install, sandbox install,
/// `uninstall_all_hooks`) dispatch through one field instead of matching agent
/// names. An agent has at most one of `hook_config` or `sidecar_hooks`.
#[derive(Debug)]
pub struct SidecarHooks {
    /// Config path relative to the home directory for a host session
    /// (e.g. `.hermes/config.yaml`).
    pub host_config_subpath: &'static str,
    /// Config path relative to the home directory for a sandboxed session
    /// (e.g. `.hermes/sandbox/config.yaml`). The `sandbox` segment mirrors the
    /// container staging dir. Empty (and unused) for `host_only` agents.
    pub sandbox_config_subpath: &'static str,
    /// Write AoE status hooks into the config file at the given path. The
    /// `target` parameter selects which `{base}` is baked into the hook
    /// command string (`/tmp/aoe-hooks-<euid>` for host, `/tmp/aoe-hooks` for
    /// sandbox; see `crate::hooks::HookInstallTarget`).
    pub install: fn(&std::path::Path, crate::hooks::HookInstallTarget) -> anyhow::Result<()>,
    /// Remove AoE status hooks from the config file at the given path.
    /// Returns whether anything was changed.
    pub uninstall: fn(&std::path::Path) -> anyhow::Result<bool>,
    /// Optional host-only follow-up run after a successful host install
    /// (e.g. kiro promotes its `aoe-hooks` agent to the active default).
    pub post_install_host: Option<fn()>,
    /// On-disk format of the sidecar's config file. Drives marker-presence
    /// walker dispatch in [`crate::hooks::has_aoe_marker`].
    pub format: SidecarFormat,
}

/// On-disk format of a sidecar agent's config file. Drives
/// marker-presence walker dispatch in [`crate::hooks::has_aoe_marker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidecarFormat {
    /// Settl `[[hooks]]` table in `.settl/config.toml`.
    SettlToml,
    /// Hermes `hooks: { event: [...] }` map in `.hermes/config.yaml` (or
    /// `.hermes/sandbox/config.yaml`).
    HermesYaml,
    /// Kiro per-agent JSON with a flat `hooks.{event}: [{command, ...}]`
    /// shape under `.kiro/...` agent files.
    KiroJson,
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
    /// Single argv token that runs this agent non-interactively (one-shot),
    /// printing the model's response to stdout and exiting (e.g. claude `-p`,
    /// codex `exec`, opencode `run`, gemini `-p`). It is exactly one token,
    /// placed immediately before the prompt argument, and must NOT contain a
    /// `{}` placeholder (the prompt is passed as its own argv element, never
    /// interpolated). `None` means the agent has no known one-shot mode, so
    /// smart session rename is skipped for it. See `session::smart_rename`.
    pub oneshot_flag: Option<&'static str>,
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
    /// Sidecar hook installer for agents whose config format the generic
    /// `hook_config` path cannot emit (settl/hermes/kiro). Mutually exclusive
    /// with `hook_config`.
    pub sidecar_hooks: Option<SidecarHooks>,
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
/// (`/tmp/aoe-hooks-<euid>/<id>/session_id`) is updated whenever Claude rotates
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

/// Codex hook events. AoE installs these into `~/.codex/hooks.json`.
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
        oneshot_flag: Some("-p"),
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
            config_dir_env_var: Some("CLAUDE_CONFIG_DIR"),
            events: CLAUDE_HOOK_EVENTS,
            format: HookFormat::JsonSettings,
        }),
        sidecar_hooks: None,
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
        oneshot_flag: Some("run"),
        binary: "opencode",
        aliases: &["open-code"],
        detection: DetectionMethod::Which("opencode"),
        yolo: Some(YoloMode::EnvVar("OPENCODE_PERMISSION", r#"{"*":"allow"}"#)),
        instruction_flag: None,
        set_default_command: true,
        detect_status: status_detection::detect_opencode_status,
        container_env: &[],
        hook_config: None,
        sidecar_hooks: None,
        resume_strategy: ResumeStrategy::Flag("--session"),
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "curl -fsSL https://opencode.ai/install | bash",
    },
    AgentDef {
        name: "vibe",
        oneshot_flag: None,
        binary: "vibe",
        aliases: &["mistral-vibe"],
        detection: DetectionMethod::RunWithArg("vibe", "--version"),
        yolo: Some(YoloMode::CliFlag("--agent auto-approve")),
        instruction_flag: None,
        set_default_command: false,
        detect_status: status_detection::detect_vibe_status,
        container_env: &[],
        hook_config: None,
        sidecar_hooks: None,
        resume_strategy: ResumeStrategy::Flag("--resume"),
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "pip install mistral-vibe",
    },
    AgentDef {
        name: "codex",
        oneshot_flag: Some("exec"),
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
            settings_rel_path: ".codex/hooks.json",
            // Codex's config dir resolves via `CODEX_HOME`, not a generic
            // `config_dir_env_var`; the `CodexJson` writer handles that itself.
            config_dir_env_var: None,
            events: CODEX_HOOK_EVENTS,
            format: HookFormat::CodexJson,
        }),
        sidecar_hooks: None,
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
        oneshot_flag: Some("-p"),
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
            config_dir_env_var: None,
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
            format: HookFormat::JsonSettings,
        }),
        sidecar_hooks: None,
        resume_strategy: ResumeStrategy::Flag("--resume"),
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "npm install -g @google/gemini-cli",
    },
    AgentDef {
        name: "cursor",
        oneshot_flag: None,
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
            config_dir_env_var: Some("CURSOR_CONFIG_DIR"),
            events: CURSOR_HOOK_EVENTS,
            format: HookFormat::JsonSettings,
        }),
        sidecar_hooks: None,
        resume_strategy: ResumeStrategy::Unsupported,
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "see https://docs.cursor.com/cli",
    },
    AgentDef {
        name: "copilot",
        oneshot_flag: None,
        binary: "copilot",
        aliases: &["github-copilot"],
        detection: DetectionMethod::Which("copilot"),
        yolo: Some(YoloMode::CliFlag("--yolo")),
        instruction_flag: None,
        set_default_command: false,
        detect_status: status_detection::detect_copilot_status,
        container_env: &[("COPILOT_CONFIG_DIR", "/root/.copilot")],
        hook_config: None,
        sidecar_hooks: None,
        resume_strategy: ResumeStrategy::Unsupported,
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "see https://docs.github.com/en/copilot/github-copilot-in-the-cli",
    },
    AgentDef {
        name: "pi",
        oneshot_flag: None,
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
        sidecar_hooks: None,
        resume_strategy: ResumeStrategy::Flag("--session"),
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "npm install -g @earendil-works/pi-coding-agent",
    },
    AgentDef {
        name: "droid",
        oneshot_flag: None,
        binary: "droid",
        aliases: &["factory-droid"],
        detection: DetectionMethod::Which("droid"),
        yolo: Some(YoloMode::CliFlag("--skip-permissions-unsafe")),
        instruction_flag: None,
        set_default_command: false,
        detect_status: status_detection::detect_droid_status,
        container_env: &[],
        hook_config: None,
        sidecar_hooks: None,
        resume_strategy: ResumeStrategy::Unsupported,
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "npm install -g droid",
    },
    AgentDef {
        name: "settl",
        oneshot_flag: None,
        binary: "settl",
        aliases: &["settlers", "catan"],
        detection: DetectionMethod::Which("settl"),
        yolo: Some(YoloMode::AlwaysYolo),
        instruction_flag: None,
        set_default_command: false,
        detect_status: status_detection::detect_settl_status,
        container_env: &[],
        // settl uses TOML config (`[[hooks]]` entries), not the JSON
        // settings.json schema, so it installs via a sidecar hook. host_only,
        // so the sandbox subpath is unused.
        hook_config: None,
        sidecar_hooks: Some(SidecarHooks {
            host_config_subpath: ".settl/config.toml",
            sandbox_config_subpath: "",
            install: crate::hooks::install_settl_hooks,
            uninstall: crate::hooks::uninstall_settl_hooks,
            post_install_host: None,
            format: SidecarFormat::SettlToml,
        }),
        resume_strategy: ResumeStrategy::Unsupported,
        host_only: true,
        send_keys_enter_delay_ms: 0,
        install_hint: "brew install --cask mozilla-ai/tap/settl",
    },
    AgentDef {
        name: "hermes",
        oneshot_flag: None,
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
        // JSON settings.json schema shared by Claude/Cursor/Gemini, so it
        // installs via a sidecar hook rather than hook_config.
        hook_config: None,
        sidecar_hooks: Some(SidecarHooks {
            host_config_subpath: ".hermes/config.yaml",
            sandbox_config_subpath: ".hermes/sandbox/config.yaml",
            install: crate::hooks::install_hermes_hooks,
            uninstall: crate::hooks::uninstall_hermes_hooks,
            post_install_host: None,
            format: SidecarFormat::HermesYaml,
        }),
        resume_strategy: ResumeStrategy::Flag("--resume"),
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint:
            "curl -fsSL https://raw.githubusercontent.com/NousResearch/hermes-agent/main/scripts/install.sh | bash",
    },
    AgentDef {
        name: "kiro",
        oneshot_flag: None,
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
        // by Claude/Cursor/Gemini, so it installs via a sidecar hook. Status
        // comes from the hook sidecar file written by install_kiro_hooks; the
        // pane stub is unused. post_install_host promotes the aoe-hooks agent
        // to Kiro's active default.
        hook_config: None,
        sidecar_hooks: Some(SidecarHooks {
            host_config_subpath: crate::hooks::KIRO_HOOKS_AGENT_FILE,
            sandbox_config_subpath: ".kiro/sandbox/agents/aoe-hooks.json",
            install: crate::hooks::install_kiro_hooks,
            uninstall: crate::hooks::uninstall_kiro_hooks,
            post_install_host: Some(crate::hooks::set_kiro_default_agent_if_builtin),
            format: SidecarFormat::KiroJson,
        }),
        resume_strategy: ResumeStrategy::Flag("--resume-id"),
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "curl -fsSL https://cli.kiro.dev/install | bash",
    },
    AgentDef {
        name: "qwen",
        oneshot_flag: None,
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
            config_dir_env_var: None,
            events: QWEN_HOOK_EVENTS,
            format: HookFormat::JsonSettings,
        }),
        sidecar_hooks: None,
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
        oneshot_flag: None,
        binary: "agy",
        aliases: &["agy"],
        detection: DetectionMethod::Which("agy"),
        yolo: Some(YoloMode::CliFlag("--dangerously-skip-permissions")),
        instruction_flag: None,
        set_default_command: false,
        detect_status: status_detection::detect_antigravity_status,
        container_env: &[],
        hook_config: None,
        sidecar_hooks: None,
        resume_strategy: ResumeStrategy::Unsupported,
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "curl -fsSL https://antigravity.google/cli/install.sh | bash",
    },
];

/// Look up an agent by canonical name.
impl AgentDef {
    /// Extra argv tokens inserted between the one-shot flag and the prompt for a
    /// one-shot (smart-rename) title call. These are static, never user input,
    /// so the no-injection contract (prompt stays the final argv element) holds.
    ///
    /// Codex's `exec` refuses to run outside a trusted git repo
    /// ("Not inside a trusted directory and --skip-git-repo-check was not
    /// specified", exit 1), so a one-shot in a scratch or other non-repo session
    /// cwd fails. `--skip-git-repo-check` lets the title call run anywhere; the
    /// title task does not touch the repo, so skipping the check is safe.
    pub fn oneshot_extra_args(&self) -> &'static [&'static str] {
        match self.name {
            "codex" => &["--skip-git-repo-check"],
            _ => &[],
        }
    }
}

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

/// Names of built-in agents that can run a one-shot title call (a non-`None`
/// `oneshot_flag`). The smart-rename agent picker lists these, since only
/// these agents can be used for the one-shot rename.
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
    fn test_oneshot_flags_are_single_tokens_without_placeholders() {
        // The smart-rename safety contract: a non-None oneshot_flag is exactly
        // one argv token placed before the prompt, and never interpolates the
        // prompt. Keep future agent additions from weakening that.
        for agent in AGENTS {
            let Some(flag) = agent.oneshot_flag else {
                continue;
            };
            assert_eq!(
                flag,
                flag.trim(),
                "agent '{}' one-shot flag must not have surrounding whitespace",
                agent.name
            );
            assert_eq!(
                flag.split_whitespace().count(),
                1,
                "agent '{}' one-shot flag must be exactly one argv token",
                agent.name
            );
            assert!(
                !flag.contains("{}"),
                "agent '{}' one-shot flag must not interpolate the prompt",
                agent.name
            );
        }
    }

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

    #[test]
    fn test_all_hook_configs_declare_expected_format() {
        // Adding or changing an agent's hook format requires updating both
        // this list and the declaration in `AGENTS`. The dispatch in
        // `crate::hooks::iter_hook_targets_in` is keyed off this field, so
        // drift here is a behavior change.
        let expected: &[(&str, HookFormat)] = &[
            ("claude", HookFormat::JsonSettings),
            ("codex", HookFormat::CodexJson),
            ("gemini", HookFormat::JsonSettings),
            ("cursor", HookFormat::JsonSettings),
            ("qwen", HookFormat::JsonSettings),
        ];
        for (name, fmt) in expected {
            let agent = get_agent(name).unwrap_or_else(|| panic!("missing agent {name}"));
            let cfg = agent
                .hook_config
                .as_ref()
                .unwrap_or_else(|| panic!("agent {name} must have hook_config"));
            assert_eq!(cfg.format, *fmt, "agent {name} hook format must be {fmt:?}");
        }
        let declared: Vec<&str> = AGENTS
            .iter()
            .filter(|a| a.hook_config.is_some())
            .map(|a| a.name)
            .collect();
        let expected_names: Vec<&str> = expected.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            declared, expected_names,
            "hook_config agent set drifted; update test_all_hook_configs_declare_expected_format"
        );
    }

    #[test]
    fn test_all_sidecar_hooks_declare_expected_format() {
        // Mirror of `test_all_hook_configs_declare_expected_format` for the
        // sidecar path. The dispatch in `crate::hooks::has_aoe_marker` is
        // keyed off this field.
        let expected: &[(&str, SidecarFormat)] = &[
            ("settl", SidecarFormat::SettlToml),
            ("hermes", SidecarFormat::HermesYaml),
            ("kiro", SidecarFormat::KiroJson),
        ];
        for (name, fmt) in expected {
            let agent = get_agent(name).unwrap_or_else(|| panic!("missing agent {name}"));
            let sidecar = agent
                .sidecar_hooks
                .as_ref()
                .unwrap_or_else(|| panic!("agent {name} must have sidecar_hooks"));
            assert_eq!(
                sidecar.format, *fmt,
                "agent {name} sidecar format must be {fmt:?}"
            );
        }
        let declared: Vec<&str> = AGENTS
            .iter()
            .filter(|a| a.sidecar_hooks.is_some())
            .map(|a| a.name)
            .collect();
        let expected_names: Vec<&str> = expected.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            declared, expected_names,
            "sidecar_hooks agent set drifted; update test_all_sidecar_hooks_declare_expected_format"
        );
    }

    #[test]
    fn test_hook_config_and_sidecar_hooks_are_mutually_exclusive() {
        // `SidecarHooks` doc states the two are mutually exclusive. Lock
        // the invariant so a future agent does not silently get hooks
        // installed by both paths.
        for agent in AGENTS {
            assert!(
                !(agent.hook_config.is_some() && agent.sidecar_hooks.is_some()),
                "agent {} must not declare both hook_config and sidecar_hooks",
                agent.name
            );
        }
    }
}
