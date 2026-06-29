//! Server-side per-agent capability and naming profiles.
//!
//! The structured view historically text-matched against `claude-agent-acp`'s tool
//! titles, `_meta.claudeCode` namespace, and `/clear` slash command. Other
//! ACP adapters (codex-acp, `opencode acp`, `gemini --acp`, vibe-acp,
//! pi-acp) have their own conventions. This module owns the subset of
//! per-agent data the Rust server needs: subagent linkage namespace,
//! conversation-reset slash aliases, and capability gates for the few
//! semantic events the server synthesises from tool calls (ExitPlanMode
//! to Plan, ScheduleWakeup to WakeupScheduled).
//!
//! Frontend card classification lives in `web/src/lib/agentProfiles.ts`;
//! the two stay aligned by name. Adding a new agent: file an entry here,
//! mirror in TS, document in `docs/acp/multi-agent.md`.

/// Per-agent server-side profile. Static; resolved by registry key
/// (e.g. `"claude"`, `"codex"`, `"opencode"`, `"gemini"`).
#[derive(Debug, Clone)]
pub struct AgentProfile {
    /// Registry key. Matches `AgentRegistry` (`src/acp/agent_registry.rs`).
    pub key: &'static str,
    /// `_meta.<namespace>.parentToolUseId` lookup order for subagent
    /// linkage. Empty when the agent's parent-child linkage is unknown;
    /// indentation simply doesn't render rather than guessing a
    /// namespace and producing phantom hierarchies.
    pub parent_meta_namespaces: &'static [&'static str],
    /// Slash commands that reset the conversation. Matched against the
    /// user's prompt prefix in `supervisor::is_clear_command`. Empty
    /// for agents whose reset semantic isn't a slash command (or isn't
    /// known yet).
    pub clear_aliases: &'static [&'static str],
    /// When true, the server synthesises a `PlanUpdated` event from a
    /// `kind: switch_mode` tool call (Claude's ExitPlanMode shape).
    /// Other agents that change modes shouldn't fire empty Plans.
    pub supports_exit_plan_mode: bool,
    /// When true, the server synthesises a `WakeupScheduled` event from
    /// a tool call titled `"ScheduleWakeup"`. Specific to Claude's
    /// `/loop` dynamic-pacing flow.
    pub supports_wakeup_tools: bool,
    /// ACP session-mode id that means "bypass all permission prompts"
    /// (the wizard's "Auto-approve" / profile `yolo_mode_default`). Each
    /// adapter names this differently: claude-agent-acp advertises
    /// `bypassPermissions`, codex-acp advertises `agent-full-access`, gemini-cli
    /// advertises `yolo`. The supervisor sends this id via
    /// `session/set_mode` immediately after spawn (see
    /// `supervisor::spawn_inner`). `None` for adapters with no known
    /// bypass mode: YOLO then stays a best-effort no-op and the session
    /// keeps the adapter's default mode rather than guessing an id the
    /// adapter would reject. See #1142.
    pub yolo_mode_id: Option<&'static str>,
}

impl AgentProfile {
    /// True when `text` matches any of this profile's clear-conversation
    /// slash aliases, tolerating surrounding whitespace and a trailing
    /// argument cluster.
    pub fn is_clear_command(&self, text: &str) -> bool {
        let trimmed = text.trim();
        for alias in self.clear_aliases {
            if trimmed == *alias {
                return true;
            }
            if let Some(rest) = trimmed.strip_prefix(*alias) {
                if rest.starts_with(char::is_whitespace) {
                    return true;
                }
            }
        }
        false
    }

    /// Read a parent tool-call id from an ACP `_meta` blob, trying each
    /// namespace this profile knows about. Returns `None` when no
    /// namespace matches or the value isn't a string.
    pub fn parent_tool_use_id_from_meta(
        &self,
        meta: &Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Option<String> {
        let map = meta.as_ref()?;
        for namespace in self.parent_meta_namespaces {
            if let Some(v) = map
                .get(*namespace)
                .and_then(|ns| ns.get("parentToolUseId"))
                .and_then(|v| v.as_str())
            {
                return Some(v.to_string());
            }
        }
        None
    }

    /// True iff the agent surfaces session-start memory recall through
    /// the tool channel with the `_meta.claudeCode.toolName` namespace
    /// claude-agent-acp adopted in v0.37.0 (upstream #703). Other
    /// agents don't emit this shape today; gating the classifier off
    /// the profile prevents accidental matches against unrelated
    /// custom tool metadata.
    pub fn supports_memory_recall_tool(&self) -> bool {
        self.parent_meta_namespaces.contains(&"claudeCode")
    }
}

/// Claude via `claude-agent-acp`. Reference profile; verified against
/// the adapter source at `~/.nvm/.../@agentclientprotocol/claude-agent-acp/dist/tools.js`.
pub const CLAUDE: AgentProfile = AgentProfile {
    key: "claude",
    parent_meta_namespaces: &["claudeCode"],
    clear_aliases: &["/clear"],
    supports_exit_plan_mode: true,
    supports_wakeup_tools: true,
    yolo_mode_id: Some("bypassPermissions"),
};

/// Legacy alias key carried by older session records (`agent_name="claude-code"`).
/// Same shape as `CLAUDE`.
pub const CLAUDE_CODE: AgentProfile = AgentProfile {
    key: "claude-code",
    ..CLAUDE
};

/// OpenAI Codex CLI via `@agentclientprotocol/codex-acp`. `/new` is the Codex
/// CLI convention for starting a fresh conversation. No TodoWrite,
/// Skill, plan mode, or ScheduleWakeup in Codex's tool surface.
pub const CODEX: AgentProfile = AgentProfile {
    key: "codex",
    parent_meta_namespaces: &[],
    clear_aliases: &["/new"],
    supports_exit_plan_mode: false,
    supports_wakeup_tools: false,
    // @agentclientprotocol/codex-acp advertises its bypass preset as the
    // `agent-full-access` session mode (read-only / agent /
    // agent-full-access), not Claude's `bypassPermissions`.
    yolo_mode_id: Some("agent-full-access"),
};

/// SST OpenCode via native `opencode acp`. OpenCode's `task` tool can
/// spawn subagents, but its parent-child linkage convention over ACP
/// isn't documented; leave indentation off until observed rather than
/// guessing a `_meta` namespace.
pub const OPENCODE: AgentProfile = AgentProfile {
    key: "opencode",
    parent_meta_namespaces: &[],
    clear_aliases: &["/new"],
    supports_exit_plan_mode: false,
    supports_wakeup_tools: false,
    // OpenCode's bypass-mode id over ACP is unverified; leave YOLO a no-op
    // until observed rather than guessing an id the adapter would reject.
    yolo_mode_id: None,
};

/// Google Gemini CLI via native `gemini --acp`. Gemini's `/restore` is
/// a session-revert command, not a conversation-clear boundary; leave
/// clear aliases empty rather than corrupting transcript segmentation.
pub const GEMINI: AgentProfile = AgentProfile {
    key: "gemini",
    parent_meta_namespaces: &[],
    clear_aliases: &[],
    supports_exit_plan_mode: false,
    supports_wakeup_tools: false,
    // gemini-cli surfaces its YOLO approval mode over `gemini --acp` with
    // the `yolo` id (see the CurrentModeUpdate mapping in acp_client.rs).
    yolo_mode_id: Some("yolo"),
};

/// Mistral Vibe via bundled `vibe-acp`. Defaults until verified.
pub const VIBE: AgentProfile = AgentProfile {
    key: "vibe",
    parent_meta_namespaces: &[],
    clear_aliases: &[],
    supports_exit_plan_mode: false,
    supports_wakeup_tools: false,
    yolo_mode_id: None,
};

/// Pi coding agent via `pi-acp`. Defaults until verified.
pub const PI: AgentProfile = AgentProfile {
    key: "pi",
    parent_meta_namespaces: &[],
    clear_aliases: &[],
    supports_exit_plan_mode: false,
    supports_wakeup_tools: false,
    yolo_mode_id: None,
};

/// aoe's bundled multi-provider agent. Treated as Claude-equivalent
/// for now (Vercel AI SDK 6 with Claude as one of the providers); the
/// claude_capabilities subset is the safest reference until aoe-agent
/// has its own conventions.
pub const AOE_AGENT: AgentProfile = AgentProfile {
    key: "aoe-agent",
    ..CLAUDE
};

/// Permissive default for unknown registry keys: no claude-specific
/// gates fire, no clear aliases match, no parent-meta lookup. The
/// structured view still renders generic tool cards via ACP `ToolKind` and
/// shows whatever the agent emits.
pub const DEFAULT: AgentProfile = AgentProfile {
    key: "default",
    parent_meta_namespaces: &[],
    clear_aliases: &[],
    supports_exit_plan_mode: false,
    supports_wakeup_tools: false,
    yolo_mode_id: None,
};

/// Resolve a static profile by registry key. Returns `DEFAULT` for
/// unknown keys.
pub fn resolve(key: &str) -> &'static AgentProfile {
    match key {
        "claude" => &CLAUDE,
        "claude-code" => &CLAUDE_CODE,
        "codex" => &CODEX,
        "opencode" => &OPENCODE,
        "gemini" => &GEMINI,
        "vibe" => &VIBE,
        "pi" => &PI,
        "aoe-agent" => &AOE_AGENT,
        _ => &DEFAULT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_known_agents() {
        assert_eq!(resolve("claude").key, "claude");
        assert_eq!(resolve("claude-code").key, "claude-code");
        assert_eq!(resolve("codex").key, "codex");
        assert_eq!(resolve("opencode").key, "opencode");
        assert_eq!(resolve("gemini").key, "gemini");
        assert_eq!(resolve("vibe").key, "vibe");
        assert_eq!(resolve("pi").key, "pi");
        assert_eq!(resolve("aoe-agent").key, "aoe-agent");
    }

    #[test]
    fn resolve_falls_back_to_default() {
        assert_eq!(resolve("").key, "default");
        assert_eq!(resolve("unknown-agent").key, "default");
    }

    #[test]
    fn yolo_mode_id_is_adapter_specific() {
        // Each adapter names its bypass-all-permissions mode differently;
        // the supervisor sends exactly this id via session/set_mode.
        assert_eq!(resolve("claude").yolo_mode_id, Some("bypassPermissions"));
        // Inherited from CLAUDE via `..CLAUDE`.
        assert_eq!(
            resolve("claude-code").yolo_mode_id,
            Some("bypassPermissions")
        );
        assert_eq!(resolve("aoe-agent").yolo_mode_id, Some("bypassPermissions"));
        // Regression for #1142 and the @agentclientprotocol/codex-acp
        // migration: codex's bypass preset is `agent-full-access`, not
        // Claude's `bypassPermissions` or the old Zed adapter's `full-access`.
        // A stale id is dropped by the not-advertised guard, leaving codex
        // prompting for approvals despite yolo_mode_default.
        assert_eq!(resolve("codex").yolo_mode_id, Some("agent-full-access"));
        assert_eq!(resolve("gemini").yolo_mode_id, Some("yolo"));
        // Adapters with no verified bypass mode keep YOLO a no-op.
        assert_eq!(resolve("opencode").yolo_mode_id, None);
        assert_eq!(resolve("vibe").yolo_mode_id, None);
        assert_eq!(resolve("pi").yolo_mode_id, None);
        assert_eq!(resolve("unknown-agent").yolo_mode_id, None);
    }

    #[test]
    fn is_clear_command_per_profile() {
        assert!(CLAUDE.is_clear_command("/clear"));
        assert!(CLAUDE.is_clear_command("  /clear  "));
        assert!(CLAUDE.is_clear_command("/clear --hard"));
        assert!(!CLAUDE.is_clear_command("/new"));

        assert!(CODEX.is_clear_command("/new"));
        assert!(!CODEX.is_clear_command("/clear"));

        assert!(OPENCODE.is_clear_command("/new"));
        assert!(!OPENCODE.is_clear_command("/clear"));

        // Gemini has no clear alias; nothing matches.
        assert!(!GEMINI.is_clear_command("/clear"));
        assert!(!GEMINI.is_clear_command("/new"));
        assert!(!GEMINI.is_clear_command("/restore"));
    }

    #[test]
    fn is_clear_command_rejects_partial_matches() {
        assert!(!CLAUDE.is_clear_command("clear"));
        assert!(!CLAUDE.is_clear_command("/cleart"));
        assert!(!CLAUDE.is_clear_command("hello /clear world"));
        assert!(!CLAUDE.is_clear_command(""));
    }

    #[test]
    fn parent_tool_use_id_from_meta_reads_claudecode_for_claude() {
        let mut meta = serde_json::Map::new();
        meta.insert(
            "claudeCode".to_string(),
            serde_json::json!({ "parentToolUseId": "tc-parent-7" }),
        );
        assert_eq!(
            CLAUDE.parent_tool_use_id_from_meta(&Some(meta)),
            Some("tc-parent-7".to_string())
        );
    }

    #[test]
    fn parent_tool_use_id_from_meta_returns_none_for_unverified_agents() {
        let mut meta = serde_json::Map::new();
        meta.insert(
            "opencode".to_string(),
            serde_json::json!({ "parentToolUseId": "tc-9" }),
        );
        // Opencode's parent linkage convention is unverified; even if the
        // wire carries the value, we don't claim it until observed.
        assert!(OPENCODE.parent_tool_use_id_from_meta(&Some(meta)).is_none());
    }

    #[test]
    fn parent_tool_use_id_from_meta_returns_none_for_missing_namespace() {
        let mut meta = serde_json::Map::new();
        meta.insert(
            "otherNamespace".to_string(),
            serde_json::json!({ "parentToolUseId": "tc-x" }),
        );
        assert!(CLAUDE.parent_tool_use_id_from_meta(&Some(meta)).is_none());
    }

    #[test]
    fn parent_tool_use_id_from_meta_returns_none_for_non_string_value() {
        let mut meta = serde_json::Map::new();
        meta.insert(
            "claudeCode".to_string(),
            serde_json::json!({ "parentToolUseId": 42 }),
        );
        assert!(CLAUDE.parent_tool_use_id_from_meta(&Some(meta)).is_none());
    }

    #[test]
    fn parent_tool_use_id_from_meta_returns_none_for_none_meta() {
        assert!(CLAUDE.parent_tool_use_id_from_meta(&None).is_none());
    }

    #[test]
    fn capability_flags_only_set_for_claude_family() {
        for profile in [&CLAUDE, &CLAUDE_CODE, &AOE_AGENT] {
            assert!(profile.supports_exit_plan_mode);
            assert!(profile.supports_wakeup_tools);
        }
        for profile in [&CODEX, &OPENCODE, &GEMINI, &VIBE, &PI, &DEFAULT] {
            assert!(!profile.supports_exit_plan_mode, "{}", profile.key);
            assert!(!profile.supports_wakeup_tools, "{}", profile.key);
        }
    }
}
