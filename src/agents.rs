//! Centralized agent registry.
//!
//! All per-agent metadata lives here. Adding a new agent means adding one
//! `AgentDef` entry to `AGENTS` and writing a status detection function.

use crate::session::Status;
use crate::tmux::status_detection;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Status values a hook may write to AoE's hook sidecar file.
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

/// How an agent forks an existing session from the CLI: resume the parent's
/// conversation but write the continuation to a NEW, independent session,
/// leaving the original transcript untouched. Distinct from
/// [`ResumeStrategy`], which continues the SAME session in place.
pub enum ForkStrategy {
    /// Claude Code: `--resume <parent> --fork-session --session-id <child>`.
    /// AoE pre-pins `<child>` so the forked id is known and durable before
    /// launch (no async capture window). Verified to compose live.
    ClaudeFork,
    /// Codex CLI: `codex fork <parent>` subcommand (mints a new id).
    CodexFork,
    /// A single flag appended when forking, used alongside the agent's normal
    /// resume flag (e.g. opencode `--session <parent> --fork`).
    Flag(&'static str),
    /// Agent cannot fork a session.
    Unsupported,
}

/// A single hook event that AoE registers in an agent's settings file.
#[derive(Debug)]
pub struct HookEvent {
    /// Event name as the agent expects it (e.g. `"PreToolUse"` for Claude Code).
    pub name: &'static str,
    /// Optional matcher pattern (e.g. `"permission_prompt|elicitation_dialog"`).
    pub matcher: Option<&'static str>,
    /// AoE status to write when this event fires.
    pub status: Option<HookStatus>,
    /// When `true`, install an additional hook command that extracts
    /// `session_id` from the agent's stdin JSON payload and writes it to
    /// `/tmp/aoe-hooks-<euid>/<AOE_INSTANCE_ID>/session_id`.
    pub session_id_capture: bool,
    /// Tool names whose invocation blocks on the user for the tool's entire
    /// execution (e.g. Claude's `AskUserQuestion` selection UI). When
    /// non-empty on a status event, the generated hook command inspects the
    /// payload's `tool_name` and writes `waiting` for these tools instead of
    /// the event's status, so the session reads as blocked the moment the
    /// prompt renders rather than sticking on the last `running` write.
    pub waiting_tools: &'static [&'static str],
}

/// A hook event after applying profile/global status-map overrides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedHookEvent {
    pub name: String,
    pub matcher: Option<String>,
    pub status: Option<HookStatus>,
    pub session_id_capture: bool,
    pub waiting_tools: Vec<String>,
}

/// Sidecar hook defaults for agents whose config format is not the generic
/// JSON hook schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SidecarHookEvent {
    pub name: &'static str,
    pub status: HookStatus,
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
    /// `crate::hooks::iter_hook_targets_in`, which feeds the v015 marker
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
    pub install: fn(
        &std::path::Path,
        crate::hooks::HookInstallTarget,
        &[ResolvedHookEvent],
    ) -> anyhow::Result<()>,
    /// Remove AoE status hooks from the config file at the given path.
    /// Returns whether anything was changed.
    pub uninstall: fn(&std::path::Path) -> anyhow::Result<bool>,
    /// Optional host-only follow-up run after a successful host install
    /// (e.g. kiro promotes its `aoe-hooks` agent to the active default).
    pub post_install_host: Option<fn()>,
    /// Set for CLIs whose hooks are scoped to a user-selectable named agent
    /// rather than applying globally (e.g. Kiro: `--agent NAME` loads only that
    /// agent's config, and there is no global hooks mechanism). When set and
    /// the user selected an agent, AoE installs its hooks into that agent's own
    /// config file instead of the standalone `host_config_subpath` agent, and
    /// skips `post_install_host`. `None` for agents whose hooks apply
    /// regardless of which agent is selected. See
    /// `crate::session::Instance::install_agent_status_hooks`.
    pub selected_agent_hooks: Option<SelectedAgentHooks>,
    /// On-disk format of the sidecar's config file. Drives marker-presence
    /// walker dispatch in `crate::hooks::has_aoe_marker`.
    pub format: SidecarFormat,
    /// Default hook events and statuses for this sidecar format.
    pub events: &'static [SidecarHookEvent],
}

/// How to install status hooks into a user-selected named agent, for CLIs
/// whose hooks are scoped to the selected agent (see
/// [`SidecarHooks::selected_agent_hooks`]). Keeps the flag and path convention
/// as data on the agent definition rather than a per-agent string match at the
/// install site.
#[derive(Debug)]
pub struct SelectedAgentHooks {
    /// CLI flag a user passes to choose a named agent (e.g. `"--agent"`).
    pub flag: &'static str,
    /// Absolute path, under the given agents directory, of the config file the
    /// CLI actually loads for the selected agent name. The first argument is
    /// the agents directory to resolve within (host: `$HOME/.kiro/agents`;
    /// sandbox: the staged `.kiro/sandbox/agents`), the second is the validated
    /// selected agent name. Resolves by the `name` field inside each config
    /// rather than the filename, since generator-managed agents name files
    /// `<prefix>-<name>.json`. See [`crate::hooks::resolve_kiro_agent_file`].
    pub resolve_config_file: fn(&std::path::Path, &str) -> std::path::PathBuf,
}

/// On-disk format of a sidecar agent's config file. Drives
/// marker-presence walker dispatch in `crate::hooks::has_aoe_marker`.
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
    /// Kimi Code `[[hooks]]` array of tables in `.kimi-code/config.toml`,
    /// each `{ event, command }` (matcher/timeout optional). Shares the
    /// flat-array shape with settl but lives in Kimi's runtime config file,
    /// which also holds provider/oauth settings, so its installer preserves
    /// the surrounding document.
    KimiToml,
}

/// Everything we know about a single agent CLI.
pub struct AgentDef {
    /// Canonical name: `"claude"`, `"opencode"`, etc.
    pub name: &'static str,
    /// Binary to invoke (usually same as name).
    pub binary: &'static str,
    /// Subcommand token inserted immediately after `binary` when AoE builds the
    /// default launch command (e.g. `Some("chat")` for kiro → `kiro-cli chat`).
    /// Required for CLIs whose interactive flags (yolo, `--agent`, resume) live
    /// on a subcommand rather than the top-level binary: bare
    /// `kiro-cli --trust-all-tools` is rejected with "unexpected argument",
    /// while `kiro-cli chat --trust-all-tools` parses. `None` for agents whose
    /// bare binary already accepts those flags. Only applied to the default
    /// binary path, never to a user's custom command override.
    ///
    /// Must not be combined with [`ResumeStrategy::Subcommand`]: that strategy
    /// inserts the resume token after the first whitespace token (the binary),
    /// which would land it before this launch subcommand. The pairing is
    /// rejected by `test_launch_subcommand_not_combined_with_subcommand_resume`.
    pub launch_subcommand: Option<&'static str>,
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
    /// How this agent forks a prior session into a new, independent one.
    pub fork_strategy: ForkStrategy,
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
    /// Static keystroke sequences for answering this agent's own interactive
    /// permission prompt from the sidebar, without attaching to the pane.
    /// `None` for every agent whose prompt shape hasn't been mapped yet; the
    /// respond-to-prompt action is a no-op for those agents. See
    /// `docs/development/adding-agents.md` for how to determine these
    /// sequences for a new agent.
    pub permission_response: Option<PermissionResponse>,
}

/// A tmux keystroke: either literal text sent verbatim (e.g. a menu digit) or
/// a named tmux key (e.g. `"Enter"`, `"Right"`). See `TmuxSession::send_key_tokens`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyToken {
    /// Sent as `tmux send-keys -l -- <text>` (literal, no key-name interpretation).
    Literal(&'static str),
    /// Sent as `tmux send-keys <name>`, e.g. `"Enter"`, `"Right"`.
    Named(&'static str),
}

/// The three keystroke sequences that answer an agent's own interactive
/// permission prompt, mapped once by hand per agent and never derived from
/// pane content. The user visually confirms a prompt is actually showing
/// before invoking the respond-to-prompt action; the software does not detect
/// or verify it.
#[derive(Clone, Copy, Debug)]
pub struct PermissionResponse {
    /// Keystrokes that select "allow once" / "yes" for this single request.
    pub allow: &'static [KeyToken],
    /// Keystrokes that select "allow always" / "don't ask again" for the
    /// remainder of the session.
    pub allow_always: &'static [KeyToken],
    /// Keystrokes that select "deny" / "no".
    pub deny: &'static [KeyToken],
}

/// Claude Code hook events. `SessionStart` and `UserPromptSubmit` carry
/// `session_id_capture: true` so the per-instance sidecar
/// (`/tmp/aoe-hooks-<euid>/<id>/session_id`) is updated whenever Claude rotates
/// its session UUID (`/clear`, `/new`, `--fork-session`, resume, compact).
/// `claude_poll_fn` reads this sidecar before falling back to its disk
/// scan.
///
/// `idle` has two sources, not just `Stop`. `Stop` does not fire on every
/// turn-end path: a turn killed by an API error fires `StopFailure` instead,
/// and a user interrupt fires nothing. Newer Claude Code has a further gap: a
/// "silent tool stop" (a tool result followed by no text) parks at the prompt
/// firing neither `Stop` nor `idle_prompt`. Without a second idle signal the
/// status file stays on the last `running` write and the session sticks on
/// Running. `Notification` with matcher `idle_prompt` is Claude's explicit
/// "done working, waiting for the user" signal and fires whenever Claude parks
/// at the prompt regardless of why the turn ended, so it backstops `Stop`;
/// `StopFailure` covers the API-error path deterministically. The remaining
/// gap (silent tool stop) has no hook, so it is recovered pane-side by
/// `reconcile_claude_hook_status`.
///
/// The `Notification` matchers also carry the agent-view identifiers added in
/// Claude Code 2.1.198: `agent_needs_input` (background session blocked on the
/// user → Waiting) rides the permission group, and `agent_completed`
/// (background session finished/failed → Idle) rides the `idle_prompt` group.
/// They only fire while Claude's agent view is open, so they are best-effort
/// extra coverage for that surface, not a substitute for the pane fallback.
///
/// `AskUserQuestion` blocks on the user for the tool's entire execution, but
/// emits no Waiting-mapped hook: `PreToolUse` fires (running) and nothing else
/// happens until the user answers, so the status file would stick on `running`
/// the whole time the question is on screen. `waiting_tools` on `PreToolUse`
/// makes the status command write `waiting` when the payload's `tool_name` is
/// `AskUserQuestion`, and the `PostToolUse` matcher restores `running` the
/// moment the answer lands (the rest of the turn is ordinary generation). The
/// pane-side `reconcile_claude_hook_status` stays as the backstop for hooks
/// installed before this pair existed.
const CLAUDE_HOOK_EVENTS: &[HookEvent] = &[
    HookEvent {
        name: "SessionStart",
        matcher: None,
        status: None,
        session_id_capture: true,
        waiting_tools: &[],
    },
    HookEvent {
        name: "PreToolUse",
        matcher: None,
        status: Some(HookStatus::Running),
        session_id_capture: false,
        waiting_tools: &["AskUserQuestion"],
    },
    HookEvent {
        name: "PostToolUse",
        matcher: Some("AskUserQuestion"),
        status: Some(HookStatus::Running),
        session_id_capture: false,
        waiting_tools: &[],
    },
    HookEvent {
        name: "UserPromptSubmit",
        matcher: None,
        status: Some(HookStatus::Running),
        session_id_capture: true,
        waiting_tools: &[],
    },
    HookEvent {
        name: "Stop",
        matcher: None,
        status: Some(HookStatus::Idle),
        session_id_capture: false,
        waiting_tools: &[],
    },
    HookEvent {
        name: "StopFailure",
        matcher: None,
        status: Some(HookStatus::Idle),
        session_id_capture: false,
        waiting_tools: &[],
    },
    HookEvent {
        name: "Notification",
        matcher: Some("permission_prompt|elicitation_dialog|agent_needs_input"),
        status: Some(HookStatus::Waiting),
        session_id_capture: false,
        waiting_tools: &[],
    },
    HookEvent {
        name: "Notification",
        matcher: Some("idle_prompt|agent_completed"),
        status: Some(HookStatus::Idle),
        session_id_capture: false,
        waiting_tools: &[],
    },
    HookEvent {
        name: "ElicitationResult",
        matcher: None,
        status: Some(HookStatus::Running),
        session_id_capture: false,
        waiting_tools: &[],
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
        status: Some(HookStatus::Running),
        session_id_capture: false,
        waiting_tools: &[],
    },
    HookEvent {
        name: "UserPromptSubmit",
        matcher: None,
        status: Some(HookStatus::Running),
        session_id_capture: false,
        waiting_tools: &[],
    },
    HookEvent {
        name: "Stop",
        matcher: None,
        status: Some(HookStatus::Idle),
        session_id_capture: false,
        waiting_tools: &[],
    },
    HookEvent {
        name: "Notification",
        matcher: Some("permission_prompt|elicitation_dialog"),
        status: Some(HookStatus::Waiting),
        session_id_capture: false,
        waiting_tools: &[],
    },
    HookEvent {
        name: "ElicitationResult",
        matcher: None,
        status: Some(HookStatus::Running),
        session_id_capture: false,
        waiting_tools: &[],
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
        status: Some(HookStatus::Running),
        session_id_capture: false,
        waiting_tools: &[],
    },
    HookEvent {
        name: "UserPromptSubmit",
        matcher: None,
        status: Some(HookStatus::Running),
        session_id_capture: false,
        waiting_tools: &[],
    },
    HookEvent {
        name: "PostToolUse",
        matcher: None,
        status: Some(HookStatus::Running),
        session_id_capture: false,
        waiting_tools: &[],
    },
    HookEvent {
        name: "Stop",
        matcher: None,
        status: Some(HookStatus::Idle),
        session_id_capture: false,
        waiting_tools: &[],
    },
    HookEvent {
        name: "Notification",
        matcher: Some("permission_prompt|elicitation_dialog"),
        status: Some(HookStatus::Waiting),
        session_id_capture: false,
        waiting_tools: &[],
    },
];

/// Codex hook events. AoE installs these into `~/.codex/hooks.json`.
const CODEX_HOOK_EVENTS: &[HookEvent] = &[
    HookEvent {
        name: "SessionStart",
        matcher: None,
        status: Some(HookStatus::Idle),
        session_id_capture: false,
        waiting_tools: &[],
    },
    HookEvent {
        name: "UserPromptSubmit",
        matcher: None,
        status: Some(HookStatus::Running),
        session_id_capture: false,
        waiting_tools: &[],
    },
    HookEvent {
        name: "PreToolUse",
        matcher: None,
        status: Some(HookStatus::Running),
        session_id_capture: false,
        waiting_tools: &[],
    },
    HookEvent {
        name: "PermissionRequest",
        matcher: None,
        status: Some(HookStatus::Waiting),
        session_id_capture: false,
        waiting_tools: &[],
    },
    HookEvent {
        name: "PostToolUse",
        matcher: None,
        status: Some(HookStatus::Running),
        session_id_capture: false,
        waiting_tools: &[],
    },
    HookEvent {
        name: "Stop",
        matcher: None,
        status: Some(HookStatus::Idle),
        session_id_capture: false,
        waiting_tools: &[],
    },
];

pub(crate) const SETTL_SIDECAR_EVENTS: &[SidecarHookEvent] = &[
    SidecarHookEvent {
        name: "TurnStarted",
        status: HookStatus::Running,
    },
    SidecarHookEvent {
        name: "WaitingForHuman",
        status: HookStatus::Waiting,
    },
    SidecarHookEvent {
        name: "GameWon",
        status: HookStatus::Idle,
    },
];

pub(crate) const HERMES_SIDECAR_EVENTS: &[SidecarHookEvent] = &[
    SidecarHookEvent {
        name: "pre_llm_call",
        status: HookStatus::Running,
    },
    SidecarHookEvent {
        name: "pre_tool_call",
        status: HookStatus::Running,
    },
    SidecarHookEvent {
        name: "post_llm_call",
        status: HookStatus::Idle,
    },
    SidecarHookEvent {
        name: "pre_approval_request",
        status: HookStatus::Waiting,
    },
    SidecarHookEvent {
        name: "post_approval_response",
        status: HookStatus::Running,
    },
    SidecarHookEvent {
        name: "on_session_end",
        status: HookStatus::Idle,
    },
];

pub(crate) const KIRO_SIDECAR_EVENTS: &[SidecarHookEvent] = &[
    SidecarHookEvent {
        name: "preToolUse",
        status: HookStatus::Running,
    },
    SidecarHookEvent {
        name: "userPromptSubmit",
        status: HookStatus::Running,
    },
    SidecarHookEvent {
        name: "stop",
        status: HookStatus::Idle,
    },
];

/// Kimi Code hook events. AoE writes these into `~/.kimi-code/config.toml`
/// as `[[hooks]]` entries. Kimi exposes dedicated permission events
/// (`PermissionRequest` / `PermissionResult`), so the waiting/running
/// transition needs no `Notification` matcher the way Claude's does.
/// `StopFailure` backstops `Stop` for the API-error turn-end path.
pub(crate) const KIMI_SIDECAR_EVENTS: &[SidecarHookEvent] = &[
    SidecarHookEvent {
        name: "UserPromptSubmit",
        status: HookStatus::Running,
    },
    SidecarHookEvent {
        name: "PreToolUse",
        status: HookStatus::Running,
    },
    SidecarHookEvent {
        name: "PermissionRequest",
        status: HookStatus::Waiting,
    },
    SidecarHookEvent {
        name: "PermissionResult",
        status: HookStatus::Running,
    },
    SidecarHookEvent {
        name: "Stop",
        status: HookStatus::Idle,
    },
    SidecarHookEvent {
        name: "StopFailure",
        status: HookStatus::Idle,
    },
];

pub const AGENTS: &[AgentDef] = &[
    AgentDef {
        name: "claude",
        oneshot_flag: Some("-p"),
        binary: "claude",
        launch_subcommand: None,
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
        fork_strategy: ForkStrategy::ClaudeFork,
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "npm install -g @anthropic-ai/claude-code",
        permission_response: Some(PermissionResponse {
            allow: &[KeyToken::Literal("1")],
            allow_always: &[KeyToken::Literal("2")],
            deny: &[KeyToken::Literal("3")],
        }),
    },
    AgentDef {
        name: "opencode",
        oneshot_flag: Some("run"),
        binary: "opencode",
        launch_subcommand: None,
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
        fork_strategy: ForkStrategy::Flag("--fork"),
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "curl -fsSL https://opencode.ai/install | bash",
        permission_response: Some(PermissionResponse {
            allow: &[KeyToken::Named("Enter")],
            allow_always: &[
                KeyToken::Named("Right"),
                KeyToken::Named("Enter"),
                KeyToken::Named("Enter"),
            ],
            deny: &[
                KeyToken::Named("Right"),
                KeyToken::Named("Right"),
                KeyToken::Named("Enter"),
            ],
        }),
    },
    AgentDef {
        name: "vibe",
        oneshot_flag: None,
        binary: "vibe",
        launch_subcommand: None,
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
        fork_strategy: ForkStrategy::Unsupported,
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "pip install mistral-vibe",
        permission_response: None,
    },
    AgentDef {
        name: "codex",
        oneshot_flag: Some("exec"),
        binary: "codex",
        launch_subcommand: None,
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
        fork_strategy: ForkStrategy::CodexFork,
        host_only: false,
        // Codex has paste-burst detection with a 120ms Enter-suppression window;
        // Enter keys arriving within that window after a character stream are
        // swallowed as newlines instead of triggering submit. 150ms > 120ms.
        send_keys_enter_delay_ms: 150,
        install_hint: "npm install -g @openai/codex",
        permission_response: None,
    },
    AgentDef {
        name: "gemini",
        oneshot_flag: Some("-p"),
        binary: "gemini",
        launch_subcommand: None,
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
                    status: Some(HookStatus::Running),
                    session_id_capture: false,
                    waiting_tools: &[],
                },
                HookEvent {
                    name: "BeforeAgent",
                    matcher: None,
                    status: Some(HookStatus::Running),
                    session_id_capture: false,
                    waiting_tools: &[],
                },
                HookEvent {
                    name: "AfterAgent",
                    matcher: None,
                    status: Some(HookStatus::Idle),
                    session_id_capture: false,
                    waiting_tools: &[],
                },
                HookEvent {
                    name: "Notification",
                    matcher: Some("ToolPermission"),
                    status: Some(HookStatus::Waiting),
                    session_id_capture: false,
                    waiting_tools: &[],
                },
            ],
            format: HookFormat::JsonSettings,
        }),
        sidecar_hooks: None,
        resume_strategy: ResumeStrategy::Flag("--resume"),
        fork_strategy: ForkStrategy::Unsupported,
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "npm install -g @google/gemini-cli",
        permission_response: None,
    },
    AgentDef {
        name: "cursor",
        oneshot_flag: None,
        binary: "agent",
        launch_subcommand: None,
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
        fork_strategy: ForkStrategy::Unsupported,
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "see https://docs.cursor.com/cli",
        permission_response: None,
    },
    AgentDef {
        name: "copilot",
        oneshot_flag: Some("-p"),
        binary: "copilot",
        launch_subcommand: None,
        aliases: &["github-copilot"],
        detection: DetectionMethod::Which("copilot"),
        yolo: Some(YoloMode::CliFlag("--yolo")),
        instruction_flag: None,
        set_default_command: false,
        detect_status: status_detection::detect_copilot_status,
        container_env: &[("COPILOT_CONFIG_DIR", "/root/.copilot")],
        hook_config: None,
        sidecar_hooks: None,
        // Copilot records its live session id (a UUID) in the `sessions` table
        // of `~/.copilot/session-store.db`; the poller captures it and resumes
        // with `copilot --session-id <id>`. `--session-id` takes a required
        // value, so the space-separated form `build_resume_flags` emits parses
        // unambiguously; `--resume[=<id>]` takes an optional value and would
        // read a space-separated id as a positional prompt instead.
        resume_strategy: ResumeStrategy::Flag("--session-id"),
        fork_strategy: ForkStrategy::Unsupported,
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "see https://docs.github.com/en/copilot/github-copilot-in-the-cli",
        permission_response: None,
    },
    AgentDef {
        name: "pi",
        oneshot_flag: None,
        binary: "pi",
        launch_subcommand: None,
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
        fork_strategy: ForkStrategy::Unsupported,
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "npm install -g @earendil-works/pi-coding-agent",
        permission_response: None,
    },
    AgentDef {
        name: "droid",
        oneshot_flag: None,
        binary: "droid",
        launch_subcommand: None,
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
        fork_strategy: ForkStrategy::Unsupported,
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "npm install -g droid",
        permission_response: None,
    },
    AgentDef {
        name: "settl",
        oneshot_flag: None,
        binary: "settl",
        launch_subcommand: None,
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
            install: crate::hooks::install_settl_hooks_with_events,
            uninstall: crate::hooks::uninstall_settl_hooks,
            post_install_host: None,
            selected_agent_hooks: None,
            format: SidecarFormat::SettlToml,
            events: SETTL_SIDECAR_EVENTS,
        }),
        resume_strategy: ResumeStrategy::Unsupported,
        fork_strategy: ForkStrategy::Unsupported,
        host_only: true,
        send_keys_enter_delay_ms: 0,
        install_hint: "brew install --cask mozilla-ai/tap/settl",
        permission_response: None,
    },
    AgentDef {
        name: "hermes",
        oneshot_flag: None,
        binary: "hermes",
        launch_subcommand: None,
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
            install: crate::hooks::install_hermes_hooks_with_events,
            uninstall: crate::hooks::uninstall_hermes_hooks,
            post_install_host: None,
            selected_agent_hooks: None,
            format: SidecarFormat::HermesYaml,
            events: HERMES_SIDECAR_EVENTS,
        }),
        resume_strategy: ResumeStrategy::Flag("--resume"),
        fork_strategy: ForkStrategy::Unsupported,
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint:
            "curl -fsSL https://raw.githubusercontent.com/NousResearch/hermes-agent/main/scripts/install.sh | bash",
        permission_response: None,
    },
    AgentDef {
        name: "kiro",
        oneshot_flag: None,
        binary: "kiro-cli",
        // Kiro's interactive flags (--trust-all-tools, --agent, --resume-id)
        // are defined on the `chat` subcommand. Bare `kiro-cli --trust-all-tools`
        // fails with "unexpected argument"; `kiro-cli chat ...` parses.
        launch_subcommand: Some("chat"),
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
            install: crate::hooks::install_kiro_hooks_with_events,
            uninstall: crate::hooks::uninstall_kiro_hooks,
            post_install_host: Some(crate::hooks::set_kiro_default_agent_if_builtin),
            // Kiro scopes hooks to the agent selected by `--agent`; when the
            // user picks their own agent, install hooks into that agent's file
            // (Kiro has no global hooks) instead of the standalone aoe-hooks
            // agent, and skip the set-default promotion above.
            selected_agent_hooks: Some(SelectedAgentHooks {
                flag: "--agent",
                resolve_config_file: crate::hooks::resolve_kiro_agent_file,
            }),
            format: SidecarFormat::KiroJson,
            events: KIRO_SIDECAR_EVENTS,
        }),
        resume_strategy: ResumeStrategy::Flag("--resume-id"),
        fork_strategy: ForkStrategy::Unsupported,
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "curl -fsSL https://cli.kiro.dev/install | bash",
        permission_response: None,
    },
    AgentDef {
        name: "qwen",
        oneshot_flag: None,
        binary: "qwen",
        launch_subcommand: None,
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
        fork_strategy: ForkStrategy::Unsupported,
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "npm install -g @qwen-code/qwen-code",
        permission_response: None,
    },
    AgentDef {
        name: "antigravity",
        oneshot_flag: None,
        binary: "agy",
        launch_subcommand: None,
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
        fork_strategy: ForkStrategy::Unsupported,
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "curl -fsSL https://antigravity.google/cli/install.sh | bash",
        permission_response: None,
    },
    AgentDef {
        name: "kimi",
        oneshot_flag: Some("-p"),
        binary: "kimi",
        launch_subcommand: None,
        aliases: &["kimi-code"],
        detection: DetectionMethod::Which("kimi"),
        yolo: Some(YoloMode::CliFlag("--yolo")),
        instruction_flag: None,
        set_default_command: false,
        detect_status: status_detection::detect_kimi_status,
        container_env: &[("KIMI_CODE_HOME", "/root/.kimi-code")],
        // Kimi Code stores hooks as `[[hooks]]` entries in its runtime
        // `config.toml` (which also holds provider/oauth settings), so it
        // installs via a sidecar hook rather than the JSON settings.json
        // path. Status comes from the hook sidecar file; the pane stub is
        // unused.
        hook_config: None,
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
        // `kimi --session <id>` resumes a prior conversation. On the host the id
        // is captured from `~/.kimi-code/session_index.jsonl` (see
        // `capture_kimi_session_id`); sandboxed sessions have no capture yet and
        // start fresh on restart, mirroring Copilot.
        resume_strategy: ResumeStrategy::Flag("--session"),
        fork_strategy: ForkStrategy::Unsupported,
        host_only: false,
        send_keys_enter_delay_ms: 0,
        install_hint: "curl -fsSL https://code.kimi.com/kimi-code/install.sh | bash",
        permission_response: None,
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

    /// Static argv tokens appended *after* the prompt for a one-shot
    /// (smart-rename) title call. Only meaningful for flag-value one-shots
    /// (`oneshot_flag` is a `-p`-style option whose value is the prompt): the
    /// CLI binds the prompt to the flag, so these trailing flags cannot be read
    /// as the prompt, and the prompt cannot be read as one of them. Copilot
    /// needs `-s` (print only the final answer, no stats) plus
    /// `--allow-all-tools --no-ask-user` so the non-interactive title call
    /// never blocks on a permission or follow-up question. These are static,
    /// never user input, so the no-injection contract holds.
    pub fn oneshot_trailing_args(&self) -> &'static [&'static str] {
        match self.name {
            "copilot" => &["-s", "--allow-all-tools", "--no-ask-user"],
            _ => &[],
        }
    }

    /// The base launch token(s) for the default (non-overridden) command:
    /// the binary, plus any `launch_subcommand` (e.g. `"kiro-cli chat"`). All
    /// subsequent flags (extra args, yolo, resume) are appended after this, so
    /// subcommand-scoped flags land on the subcommand where the CLI expects
    /// them. Agents without a `launch_subcommand` just return the binary.
    pub fn launch_base_command(&self) -> String {
        match self.launch_subcommand {
            Some(sub) => format!("{} {}", self.binary, sub),
            None => self.binary.to_string(),
        }
    }
}

pub fn get_agent(name: &str) -> Option<&'static AgentDef> {
    AGENTS.iter().find(|a| a.name == name)
}

/// Whether switching a structured-view session back to a terminal can hand
/// the live conversation to `<tool> --resume <id>` instead of starting fresh.
///
/// Requires BOTH sides of the swap to share one CLI-resumable transcript:
///
///   - `tool` is the terminal CLI that will run after the swap; it must
///     resume an existing session (claude's `--resume`).
///   - `acp_agent` is the *active* structured-view adapter (the resolved
///     `pick_agent_for_tool` name, which `switch_acp_agent` can point away
///     from the tool's default), whose captured `acp_session_id` must be an
///     id that terminal CLI reads.
///
/// Today only claude qualifies: `claude-agent-acp`'s `session/new` UUID is
/// the claude SDK session id in `~/.claude/projects/*.jsonl`, exactly what
/// `claude --resume` reads. `claude-code` is the legacy alias for the same
/// adapter. codex-acp and the bundled `aoe-agent` do not share a
/// CLI-resumable store, so a claude session whose adapter was swapped to one
/// of them does not qualify.
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

// The CLI status-map query is event-name keyed, matching the config shape.
// Duplicate built-in events with different matchers collapse to the first
// default here; resolved_hook_events still applies an override to every
// matcher variant with that event name.
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
                session_id_capture: false,
                waiting_tools: Vec::new(),
            });
        }
    }
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
            session_id_capture: event.session_id_capture,
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
            session_id_capture: false,
            waiting_tools: Vec::new(),
        })
        .collect();
    append_configured_status_events(&mut events, overrides);
    Ok(events)
}

/// Extract the agent name a user selected via `<flag> NAME` or `<flag>=NAME`
/// in a command/extra-args string (e.g. Kiro's `--agent custom-agent`). The flag
/// comes from [`SelectedAgentHooks::flag`] so the convention stays data on the
/// agent definition. Returns `None` when the flag is absent, has no value, or
/// its final occurrence carries a rejected value.
///
/// The **last** occurrence decides the result, matching how clap-based CLIs
/// (Kiro included) resolve a repeated single-value flag: `--agent a --agent b`
/// loads `b`. Crucially, a later occurrence overwrites an earlier one even when
/// its value is rejected, so `--agent good --agent ..` returns `None` rather
/// than `good`: the CLI itself would load `..` (and reject it / fall back to its
/// default), so AoE must not install hooks into `good`, an agent the CLI is not
/// running. Returning `None` makes AoE fall back to its standalone hooks agent,
/// which is what the CLI effectively does. This also gives extra-args the final
/// say over a command override when `crate::session::Instance::selected_agent_args`
/// concatenates command then extra-args.
///
/// A value is rejected by `is_safe_agent_name` (empty, `.`/`..`, leading dash,
/// or a path separator) so a parsed value can be safely joined to an agents
/// directory without path traversal. Whitespace-tokenized, which matches how AoE
/// assembles the launch string; quoted values containing spaces are not handled
/// (agent names do not contain spaces in practice).
pub fn parse_selected_agent(args: &str, flag: &str) -> Option<String> {
    let eq_prefix = format!("{flag}=");
    let mut tokens = args.split_whitespace();
    let mut selected = None;
    while let Some(tok) = tokens.next() {
        // The value of this flag occurrence: the text after `=`, the next token
        // for the space-separated form, or `None` for a dangling flag.
        let value = if let Some(rest) = tok.strip_prefix(&eq_prefix) {
            Some(rest)
        } else if tok == flag {
            tokens.next()
        } else {
            continue;
        };
        // Last occurrence wins: overwrite with this occurrence's validated
        // value, so a trailing rejected/missing value clears an earlier valid
        // one (mirroring the CLI's last-wins resolution).
        selected = value.filter(|&v| is_safe_agent_name(v)).map(str::to_string);
    }
    selected
}

/// Guard against path traversal and obvious misparses: a selected agent name is
/// joined to an agents directory, so reject empty names, `.`/`..`, anything
/// containing a path separator, and flag-shaped tokens. The leading-dash check
/// means a value-less flag (`--agent --model`) yields `None` rather than
/// treating the following flag as an agent name.
fn is_safe_agent_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.starts_with('-')
        && !name.contains('/')
        && !name.contains('\\')
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
    fn acp_transcript_cli_resumable_only_for_claude_pairings() {
        // Both sides must be claude: the terminal `claude --resume` and the
        // active claude-agent-acp adapter share ~/.claude/projects/*.jsonl.
        assert!(acp_transcript_cli_resumable("claude", "claude"));
        assert!(acp_transcript_cli_resumable("claude", "claude-code"));
        // Adapter swapped away from claude (via switch_acp_agent): the
        // acp_session_id is not a claude-resumable id.
        assert!(!acp_transcript_cli_resumable("claude", "codex"));
        assert!(!acp_transcript_cli_resumable("claude", "aoe-agent"));
        // Non-claude terminal tool: no `claude --resume` to hand off to.
        assert!(!acp_transcript_cli_resumable("codex", "codex"));
    }

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
            // The same single-token, no-interpolation contract applies to the
            // static args inserted before and after the prompt.
            for extra in agent
                .oneshot_extra_args()
                .iter()
                .chain(agent.oneshot_trailing_args())
            {
                assert!(
                    !extra.contains("{}"),
                    "agent '{}' one-shot arg '{}' must not interpolate the prompt",
                    agent.name,
                    extra
                );
                assert_eq!(
                    extra.split_whitespace().count(),
                    1,
                    "agent '{}' one-shot arg '{}' must be exactly one argv token",
                    agent.name,
                    extra
                );
            }
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
        assert_eq!(get_agent("kimi").unwrap().binary, "kimi");
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
    fn effective_status_map_merges_configured_override() {
        let mut config = crate::session::config::Config::default();
        config
            .agents
            .entry("claude".to_string())
            .or_default()
            .status_map
            .insert("Stop".to_string(), HookStatus::Error);

        let map = effective_status_map(&config, "claude").unwrap();
        assert_eq!(map.get("PreToolUse"), Some(&HookStatus::Running));
        assert_eq!(map.get("Stop"), Some(&HookStatus::Error));
        assert_eq!(map.get("Notification"), Some(&HookStatus::Waiting));
    }

    #[test]
    fn resolved_hook_events_apply_duplicate_event_override() {
        let mut config = crate::session::config::Config::default();
        config
            .agents
            .entry("claude".to_string())
            .or_default()
            .status_map
            .insert("Notification".to_string(), HookStatus::Running);

        let agent = get_agent("claude").unwrap();
        let events = resolved_hook_events(agent, &config).unwrap();
        let notification_statuses: Vec<HookStatus> = events
            .iter()
            .filter(|event| event.name == "Notification")
            .filter_map(|event| event.status)
            .collect();
        assert_eq!(
            notification_statuses,
            vec![HookStatus::Running, HookStatus::Running]
        );
    }

    #[test]
    fn status_map_adds_configured_event_for_known_agent() {
        let mut config = crate::session::config::Config::default();
        config
            .agents
            .entry("claude".to_string())
            .or_default()
            .status_map
            .insert("PreCompact".to_string(), HookStatus::Running);

        let map = effective_status_map(&config, "claude").unwrap();
        assert_eq!(map.get("PreCompact"), Some(&HookStatus::Running));

        let agent = get_agent("claude").unwrap();
        let events = resolved_hook_events(agent, &config).unwrap();
        let custom = events
            .iter()
            .find(|event| event.name == "PreCompact")
            .expect("custom event should be appended");
        assert_eq!(custom.status, Some(HookStatus::Running));
        assert!(custom.matcher.is_none());
        assert!(!custom.session_id_capture);
    }

    #[test]
    fn configured_unknown_agent_status_map_is_queryable() {
        let mut config = crate::session::config::Config::default();
        config
            .agents
            .entry("vertex".to_string())
            .or_default()
            .status_map
            .insert("session.idle".to_string(), HookStatus::Idle);

        let map = effective_status_map(&config, "vertex").unwrap();
        assert_eq!(map.get("session.idle"), Some(&HookStatus::Idle));
    }

    #[test]
    fn test_copilot_agent_definition() {
        let copilot = get_agent("copilot").unwrap();
        assert_eq!(copilot.binary, "copilot");
        assert!(matches!(
            &copilot.detection,
            DetectionMethod::Which("copilot")
        ));
        assert!(matches!(&copilot.yolo, Some(YoloMode::CliFlag("--yolo"))));
        // Copilot resumes a prior conversation with `copilot --session-id <id>`,
        // where the id is captured from `~/.copilot/session-store.db`.
        assert!(matches!(
            &copilot.resume_strategy,
            ResumeStrategy::Flag("--session-id")
        ));
        // One-shot title generation runs `copilot -p <prompt> -s
        // --allow-all-tools --no-ask-user`.
        assert_eq!(copilot.oneshot_flag, Some("-p"));
        assert_eq!(
            copilot.oneshot_trailing_args(),
            &["-s", "--allow-all-tools", "--no-ask-user"]
        );
        assert!(!copilot.host_only);
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
                "antigravity",
                "kimi"
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
        assert_eq!(resolve_tool_name("kimi"), Some("kimi"));
        assert_eq!(resolve_tool_name("kimi-code"), Some("kimi"));
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
        assert_eq!(settings_index_from_name(Some("kimi")), 15);

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
        assert_eq!(name_from_settings_index(15), Some("kimi"));
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
    fn test_kiro_launches_via_chat_subcommand() {
        // Kiro's interactive flags (--trust-all-tools, --agent, --resume-id)
        // are scoped to the `chat` subcommand, so the base command must include
        // it; bare `kiro-cli --trust-all-tools` is rejected by the CLI.
        let kiro = get_agent("kiro").unwrap();
        assert_eq!(kiro.launch_subcommand, Some("chat"));
        assert_eq!(kiro.launch_base_command(), "kiro-cli chat");
    }

    #[test]
    fn test_launch_base_command_without_subcommand_is_binary() {
        // Agents with no launch_subcommand keep their bare binary.
        let claude = get_agent("claude").unwrap();
        assert_eq!(claude.launch_subcommand, None);
        assert_eq!(claude.launch_base_command(), "claude");
    }

    #[test]
    fn test_only_kiro_uses_launch_subcommand() {
        // Lock the surface: today only kiro needs a launch subcommand. A new
        // agent that needs one must update this test deliberately.
        for agent in AGENTS {
            let expected = if agent.name == "kiro" {
                Some("chat")
            } else {
                None
            };
            assert_eq!(
                agent.launch_subcommand, expected,
                "agent '{}' launch_subcommand drifted",
                agent.name
            );
        }
    }

    #[test]
    fn test_launch_subcommand_not_combined_with_subcommand_resume() {
        // `append_resume_flags` inserts a Subcommand resume token after the
        // first whitespace token, which for a launch_subcommand agent is the
        // binary. That lands the resume token before the subcommand and produces
        // a malformed command (e.g. `kiro-cli resume <id> chat ...`). Forbid the
        // pairing until that insertion is made subcommand-aware.
        for agent in AGENTS {
            if agent.launch_subcommand.is_some() {
                assert!(
                    !matches!(agent.resume_strategy, ResumeStrategy::Subcommand(_)),
                    "agent '{}' combines launch_subcommand with ResumeStrategy::Subcommand; \
                     resume token would be inserted before the subcommand",
                    agent.name
                );
            }
        }
    }

    #[test]
    fn test_parse_selected_agent() {
        assert_eq!(
            parse_selected_agent("--agent custom-agent", "--agent"),
            Some("custom-agent".to_string())
        );
        assert_eq!(
            parse_selected_agent(
                "--trust-all-tools --agent custom-agent --model x",
                "--agent"
            ),
            Some("custom-agent".to_string())
        );
        assert_eq!(
            parse_selected_agent("--agent=custom-agent", "--agent"),
            Some("custom-agent".to_string())
        );
        // Absent flag.
        assert_eq!(parse_selected_agent("--trust-all-tools", "--agent"), None);
        assert_eq!(parse_selected_agent("", "--agent"), None);
        // Dangling flag with no value.
        assert_eq!(parse_selected_agent("--foo --agent", "--agent"), None);
        // A value-less flag followed by another flag must not capture the flag
        // as the agent name.
        assert_eq!(parse_selected_agent("--agent --model x", "--agent"), None);
        // Repeated flag: last occurrence wins, matching clap precedence.
        assert_eq!(
            parse_selected_agent("--agent first --agent second", "--agent"),
            Some("second".to_string())
        );
        // Last-wins is honored even when the trailing value is rejected: the CLI
        // would load `..` (and reject / fall back), so AoE must NOT keep `good`
        // and write hooks into an agent the CLI is not running. Returns None so
        // AoE falls back to its standalone hooks agent.
        assert_eq!(
            parse_selected_agent("--agent good --agent ..", "--agent"),
            None
        );
        // A trailing dangling flag likewise clears an earlier valid value.
        assert_eq!(
            parse_selected_agent("--agent good --agent", "--agent"),
            None
        );
        // `--agent=` (empty value) is rejected.
        assert_eq!(parse_selected_agent("--agent=", "--agent"), None);
        // Path-traversal / unsafe names are rejected.
        assert_eq!(
            parse_selected_agent("--agent ../../etc/passwd", "--agent"),
            None
        );
        assert_eq!(parse_selected_agent("--agent=a/b", "--agent"), None);
        assert_eq!(parse_selected_agent("--agent .", "--agent"), None);
        // Flag is parameterized, not hardcoded.
        assert_eq!(
            parse_selected_agent("--profile prod", "--profile"),
            Some("prod".to_string())
        );
    }

    #[test]
    fn test_kiro_declares_selected_agent_hooks() {
        // Kiro's hooks are scoped to the --agent-selected agent; the flag and
        // path convention live as data on the AgentDef, not a string match at
        // the install site.
        let kiro = get_agent("kiro").unwrap();
        let sel = kiro
            .sidecar_hooks
            .as_ref()
            .unwrap()
            .selected_agent_hooks
            .as_ref()
            .expect("kiro declares selected_agent_hooks");
        assert_eq!(sel.flag, "--agent");
        // With no matching agent file in the dir, the resolver falls back to
        // `<dir>/<name>.json` (the create-path for a brand-new user agent).
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(
            (sel.resolve_config_file)(tmp.path(), "custom-agent"),
            tmp.path().join("custom-agent.json")
        );
        // The other sidecar agents do not (their hooks apply globally).
        for name in ["settl", "hermes"] {
            assert!(
                get_agent(name)
                    .unwrap()
                    .sidecar_hooks
                    .as_ref()
                    .unwrap()
                    .selected_agent_hooks
                    .is_none(),
                "agent '{name}' should not declare selected_agent_hooks"
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
        assert_eq!(
            install_hint("kimi"),
            Some("curl -fsSL https://code.kimi.com/kimi-code/install.sh | bash")
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
            ("kimi", SidecarFormat::KimiToml),
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
    fn test_fork_strategy_is_set_for_fork_capable_agents() {
        // Only claude, codex, and opencode can fork; every other agent is
        // Unsupported. Iterating the full AGENTS slice makes a new agent with a
        // stray fork_strategy fail loudly here.
        assert!(matches!(
            get_agent("claude").unwrap().fork_strategy,
            ForkStrategy::ClaudeFork
        ));
        assert!(matches!(
            get_agent("codex").unwrap().fork_strategy,
            ForkStrategy::CodexFork
        ));
        assert!(matches!(
            get_agent("opencode").unwrap().fork_strategy,
            ForkStrategy::Flag("--fork")
        ));
        for agent in AGENTS {
            let fork_capable = matches!(agent.name, "claude" | "codex" | "opencode");
            assert_eq!(
                matches!(agent.fork_strategy, ForkStrategy::Unsupported),
                !fork_capable,
                "agent '{}' fork_strategy drifted; when adding an agent, update \
                 test_fork_strategy_is_set_for_fork_capable_agents and the agent's fork_strategy",
                agent.name
            );
        }
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
