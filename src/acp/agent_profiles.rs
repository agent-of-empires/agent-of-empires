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
    /// When true, forwarding this profile's clear aliases as text cannot
    /// produce a conversation AoE is able to resume, so the server must
    /// drive the reset itself: open a fresh `session/new` on the live
    /// worker and swap the ACP session id. Two distinct adapter defects
    /// land here.
    ///
    /// - No native handler at all: the raw text is answered as an unknown
    ///   command and the conversation keeps its full context (codex-acp has
    ///   no `/new`; adding one was declined upstream in codex-acp `#317`).
    ///   See #2979.
    /// - A native handler that withholds the new conversation id: the
    ///   context does reset, but the adapter keeps serving the pre-clear
    ///   ACP session id, so AoE cannot persist a resume target and the
    ///   post-clear conversation dies with the worker (claude-agent-acp;
    ///   upstream #906).
    ///
    /// False for adapters whose clear is native and whose post-clear
    /// resumability is either fine or simply unverified; see the per-profile
    /// comments before flipping one.
    pub clear_requires_driven_reset: bool,
    /// When true, the server synthesises a `PlanUpdated` event from a
    /// `kind: switch_mode` tool call (Claude's ExitPlanMode shape).
    /// Other agents that change modes shouldn't fire empty Plans.
    pub supports_exit_plan_mode: bool,
    /// When true, the server synthesises a `WakeupScheduled` event from
    /// a tool call titled `"ScheduleWakeup"`. Specific to Claude's
    /// `/loop` dynamic-pacing flow.
    pub supports_wakeup_tools: bool,
    /// When true, the agent emits keepalive progress pings for
    /// long-running tools under a derived id `<baseToolId>-heartbeat-<N>`
    /// (see `acp_client::is_heartbeat_tool_call_id`). Only Claude Code
    /// does this today; the ingress drop is gated on this so another
    /// adapter that legitimately names a tool `*-heartbeat-<N>` is not
    /// silenced. See #3084.
    pub emits_heartbeat_keepalives: bool,
    /// ACP session-mode id that means "bypass all permission prompts"
    /// (the wizard's "Auto-approve" / profile `yolo_mode_default`). Each
    /// adapter names this differently: claude-agent-acp advertises
    /// `bypassPermissions`, codex-acp advertises `agent-full-access`, and
    /// gemini-cli advertises `yolo`. The supervisor applies this id through
    /// the mode channel advertised at spawn (see `supervisor::spawn_inner`).
    /// `None` for adapters with no known
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
    // claude-agent-acp handles `/clear` locally ("Local-only commands"), so a
    // forwarded `/clear` does reset the model context, but it does NOT rotate
    // the ACP session id and the adapter discards the `conversation_reset`
    // stream message carrying the new conversation id (0.62.0
    // `dist/acp-agent.js`: "safe to drop"; upstream #906). So AoE is left
    // holding an id that `session/load` resolves back to the PRE-clear
    // conversation. #3083 works around that by dropping the stored id on
    // `SessionCleared`, which makes the POST-clear conversation unresumable:
    // any worker restart after a `/clear` (idle auto-stop, daemon restart)
    // starts an empty session and loses everything since the clear. Driving
    // the reset ourselves mints an id we can persist, so a later restart
    // resumes the post-clear conversation. Revisit if #906 lands a structured
    // reset result that carries the new id.
    clear_requires_driven_reset: true,
    supports_exit_plan_mode: true,
    supports_wakeup_tools: true,
    emits_heartbeat_keepalives: true,
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
    // codex-acp advertises no `new`/`clear`/reset command; a forwarded
    // `/new` is answered with "unknown command" and the conversation
    // keeps its context (adding one was declined in the upstream
    // codex-acp issue `#317`).
    // The supervisor must drive the reset via `session/new`. See #2979.
    clear_requires_driven_reset: true,
    supports_exit_plan_mode: false,
    supports_wakeup_tools: false,
    emits_heartbeat_keepalives: false,
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
    // OpenCode also maps `/new`, but whether its adapter handles the
    // text natively is unverified; keep the text-forward path until
    // observed rather than driving a reset it may already perform
    // itself (see the open question in #2979).
    clear_requires_driven_reset: false,
    supports_exit_plan_mode: false,
    supports_wakeup_tools: false,
    emits_heartbeat_keepalives: false,
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
    clear_requires_driven_reset: false,
    supports_exit_plan_mode: false,
    supports_wakeup_tools: false,
    emits_heartbeat_keepalives: false,
    // gemini-cli surfaces its YOLO approval mode over `gemini --acp` with
    // the `yolo` id (see the CurrentModeUpdate mapping in acp_client/update_events.rs).
    yolo_mode_id: Some("yolo"),
};

/// Mistral Vibe via bundled `vibe-acp`. Defaults until verified.
pub const VIBE: AgentProfile = AgentProfile {
    key: "vibe",
    parent_meta_namespaces: &[],
    clear_aliases: &[],
    clear_requires_driven_reset: false,
    supports_exit_plan_mode: false,
    supports_wakeup_tools: false,
    emits_heartbeat_keepalives: false,
    yolo_mode_id: None,
};

/// Pi coding agent via `pi-acp`. Defaults until verified.
pub const PI: AgentProfile = AgentProfile {
    key: "pi",
    parent_meta_namespaces: &[],
    clear_aliases: &[],
    clear_requires_driven_reset: false,
    supports_exit_plan_mode: false,
    supports_wakeup_tools: false,
    emits_heartbeat_keepalives: false,
    yolo_mode_id: None,
};

/// Oh My Pi via native `omp acp`. OMP advertises Default and Plan modes, but
/// approval policy is separate from that mode channel, so AoE must not invent a
/// YOLO mode id. Parent linkage metadata is unobserved and stays disabled.
pub const OMP: AgentProfile = AgentProfile {
    key: "omp",
    parent_meta_namespaces: &[],
    clear_aliases: &["/new"],
    clear_requires_driven_reset: false,
    supports_exit_plan_mode: false,
    supports_wakeup_tools: false,
    emits_heartbeat_keepalives: false,
    yolo_mode_id: None,
};

/// Kimi Code (Moonshot AI) via native `kimi acp`. Verified against the
/// binary's `acp-adapter/src/modes.ts`: it advertises the canonical
/// four-mode taxonomy (`default`, `plan`, `auto`, `yolo`), so `yolo` is
/// the bypass-all-permissions mode. Its parent/child subagent linkage
/// convention over ACP is unobserved, so indentation stays off. `/new`
/// starts a fresh conversation.
pub const KIMI: AgentProfile = AgentProfile {
    key: "kimi",
    parent_meta_namespaces: &[],
    clear_aliases: &["/new"],
    clear_requires_driven_reset: false,
    supports_exit_plan_mode: false,
    supports_wakeup_tools: false,
    emits_heartbeat_keepalives: false,
    yolo_mode_id: Some("yolo"),
};

/// PrimeIntellect Prime Agent via native `prime-agent --mode acp`. One ACP
/// session per process is an upstream limit; a second `session/new` is
/// refused rather than silently sharing state. Extensions (subagents,
/// goals, gates) travel in a reverse-domain `_meta` envelope
/// (`ai.primeintellect.prime-agent`), but only session-info envelopes are
/// documented; tool-call parent linkage is not, so indentation stays off.
/// `/new` exists as a TUI command, but its ACP-side clear semantics are
/// unobserved, so clear aliases stay empty until then.
pub const PRIME_AGENT: AgentProfile = AgentProfile {
    key: "prime-agent",
    parent_meta_namespaces: &[],
    clear_aliases: &[],
    clear_requires_driven_reset: false,
    supports_exit_plan_mode: false,
    supports_wakeup_tools: false,
    emits_heartbeat_keepalives: false,
    yolo_mode_id: None,
};

/// aoe's own multi-provider agent (Vercel AI SDK 7), at
/// `acp-worker/aoe-agent/src/index.ts`. Every field below is read off that
/// source, which is a much smaller surface than Claude's: three tools
/// (Read, Write, Bash), no `_meta` on any event, no plan mode, no
/// ScheduleWakeup, no heartbeats, and a `session/set_mode` that is a stub
/// returning `{}`. It used to inherit `..CLAUDE`, which claimed all of
/// those; the structured view then advertised subagent indentation and a
/// mode picker the adapter cannot honor. See #1904.
pub const AOE_AGENT: AgentProfile = AgentProfile {
    key: "aoe-agent",
    // The adapter sets no `_meta` on its tool_call notifications at all, so
    // there is no namespace to look up and child tool calls cannot be linked
    // to a parent. Populate this only once the adapter emits linkage.
    parent_meta_namespaces: &[],
    clear_aliases: &["/clear"],
    // Same defect class as codex-acp: the adapter has no clear handler of any
    // kind, so a forwarded `/clear` is answered as an ordinary prompt and the
    // model keeps its whole history while AoE draws a clear divider. Driving
    // the reset via `session/new` mints an id we can persist and does reset
    // the in-memory history.
    //
    // Known hole: the adapter reseeds from `${AOE_ARTIFACT_DIR}/transcript.jsonl`
    // on `session/load`, and that file still holds the pre-clear turns, so a
    // worker restart after a clear resurrects the cleared context. Fixing that
    // means truncating the transcript on reset, tracked separately.
    clear_requires_driven_reset: true,
    // No ExitPlanMode tool and no mode channel; no ScheduleWakeup or cron
    // tools. Synthesising Plan or WakeupScheduled events here would fire on
    // nothing.
    supports_exit_plan_mode: false,
    supports_wakeup_tools: false,
    emits_heartbeat_keepalives: false,
    // `session/set_mode` is `() => ({})`: it accepts any id and changes
    // nothing, and the adapter advertises no modes. Claiming Claude's
    // `bypassPermissions` made YOLO look applied when nothing was gated by
    // the adapter at all, so keep it a documented no-op.
    yolo_mode_id: None,
};

/// Permissive default for unknown registry keys: no claude-specific
/// gates fire, no clear aliases match, no parent-meta lookup. The
/// structured view still renders generic tool cards via ACP `ToolKind` and
/// shows whatever the agent emits.
pub const DEFAULT: AgentProfile = AgentProfile {
    key: "default",
    parent_meta_namespaces: &[],
    clear_aliases: &[],
    clear_requires_driven_reset: false,
    supports_exit_plan_mode: false,
    supports_wakeup_tools: false,
    emits_heartbeat_keepalives: false,
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
        "omp" => &OMP,
        "kimi" => &KIMI,
        "prime-agent" => &PRIME_AGENT,
        "aoe-agent" => &AOE_AGENT,
        _ => &DEFAULT,
    }
}

/// Whether `key` names an adapter whose approval-mode conventions have been
/// verified against its adapter source, so the automation policy may grant the
/// benign (interactive/guarded) classifications for its omitted default and
/// trusted-table mode ids (#2897). Adapters whose default/mode approval
/// behavior is not yet verified (`opencode`, `vibe`, `pi`) are deliberately
/// excluded so they fail closed to unattended, matching the classifier's
/// fail-closed principle. This is a security-policy set, narrower than "has a
/// non-[`DEFAULT`] static profile": add an adapter only once its default and
/// mode approval semantics are confirmed.
pub fn is_reviewed(key: &str) -> bool {
    matches!(
        key,
        "claude" | "claude-code" | "codex" | "gemini" | "kimi" | "aoe-agent"
    )
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
        assert_eq!(resolve("omp").key, "omp");
        assert_eq!(resolve("kimi").key, "kimi");
        assert_eq!(resolve("prime-agent").key, "prime-agent");
        assert_eq!(resolve("aoe-agent").key, "aoe-agent");
    }

    #[test]
    fn resolve_falls_back_to_default() {
        assert_eq!(resolve("").key, "default");
        assert_eq!(resolve("unknown-agent").key, "default");
    }

    #[test]
    fn is_reviewed_covers_only_verified_approval_conventions() {
        // Verified adapters get the benign automation classifications.
        for key in [
            "claude",
            "claude-code",
            "codex",
            "gemini",
            "kimi",
            "aoe-agent",
        ] {
            assert!(is_reviewed(key), "{key} should be reviewed");
        }
        // Adapters whose default/mode approval behavior is unverified fail
        // closed to unattended, and unknown keys never count as reviewed.
        for key in [
            "opencode",
            "vibe",
            "pi",
            "omp",
            "prime-agent",
            "unknown-agent",
            "",
        ] {
            assert!(!is_reviewed(key), "{key} should not be reviewed");
        }
    }

    #[test]
    fn yolo_mode_id_is_adapter_specific() {
        // Each adapter names its bypass-all-permissions mode differently;
        // the supervisor applies exactly this id through the advertised mode
        // channel.
        assert_eq!(resolve("claude").yolo_mode_id, Some("bypassPermissions"));
        // Inherited from CLAUDE via `..CLAUDE`.
        assert_eq!(
            resolve("claude-code").yolo_mode_id,
            Some("bypassPermissions")
        );
        // aoe-agent's `session/set_mode` is a stub that accepts any id and
        // gates nothing, so it advertises no bypass mode (#1904).
        assert_eq!(resolve("aoe-agent").yolo_mode_id, None);
        // Regression for #1142 and the @agentclientprotocol/codex-acp
        // migration: codex's bypass preset is `agent-full-access`, not
        // Claude's `bypassPermissions` or the old Zed adapter's `full-access`.
        // A stale id is rejected by Codex's advertised mode channel, leaving
        // the session prompting for approvals despite yolo_mode_default.
        assert_eq!(resolve("codex").yolo_mode_id, Some("agent-full-access"));
        assert_eq!(resolve("gemini").yolo_mode_id, Some("yolo"));
        // Kimi's acp-adapter advertises the canonical default/plan/auto/yolo
        // taxonomy; `yolo` is its bypass-all-permissions mode.
        assert_eq!(resolve("kimi").yolo_mode_id, Some("yolo"));
        // Adapters with no verified bypass mode keep YOLO a no-op.
        assert_eq!(resolve("opencode").yolo_mode_id, None);
        assert_eq!(resolve("vibe").yolo_mode_id, None);
        assert_eq!(resolve("pi").yolo_mode_id, None);
        assert_eq!(resolve("omp").yolo_mode_id, None);
        assert_eq!(resolve("prime-agent").yolo_mode_id, None);
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
        assert!(OMP.is_clear_command("/new"));
        assert!(!OMP.is_clear_command("/clear"));
    }

    /// Two different defects share the driven-reset remedy. codex-acp has no
    /// native `/new` at all (upstream codex-acp `#317`), so a forwarded alias
    /// is swallowed and the context survives (#2979). claude-agent-acp does
    /// handle `/clear`, but withholds the post-clear conversation id, so the
    /// text-forward path leaves the new conversation unresumable across a
    /// worker restart. Both need AoE to mint the id itself.
    ///
    /// `aoe-agent` lands in the same bucket as codex, for codex's reason
    /// rather than claude's: its adapter source handles no slash command at
    /// all, so a forwarded `/clear` reaches the model as an ordinary prompt
    /// and the conversation keeps its full history (#1904). Same standard for
    /// the unverified `/new` mappings (opencode, omp, kimi), which keep the
    /// old behavior until observed.
    #[test]
    fn clear_requires_driven_reset_for_codex_and_claude() {
        for profile in [&CODEX, &CLAUDE, &CLAUDE_CODE, &AOE_AGENT] {
            assert!(profile.clear_requires_driven_reset, "{}", profile.key);
        }
        for profile in [
            &OPENCODE,
            &GEMINI,
            &VIBE,
            &PI,
            &OMP,
            &KIMI,
            &PRIME_AGENT,
            &DEFAULT,
        ] {
            assert!(!profile.clear_requires_driven_reset, "{}", profile.key);
        }
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

        // aoe-agent emits no `_meta` on any tool_call notification, so even
        // claude's own namespace must not resolve for it (#1904). It used to,
        // via `..CLAUDE`, which promised linkage the adapter never sends.
        let mut claude_meta = serde_json::Map::new();
        claude_meta.insert(
            "claudeCode".to_string(),
            serde_json::json!({ "parentToolUseId": "tc-parent-7" }),
        );
        assert!(AOE_AGENT
            .parent_tool_use_id_from_meta(&Some(claude_meta))
            .is_none());
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
    fn deferred_profiles_keep_parent_linkage_disabled() {
        // These adapters' tool-call parent linkage is undocumented; pin the
        // conservative empty namespace so a future edit cannot silently
        // start claiming hierarchy (prime-agent's _meta envelope is
        // session-info only today).
        for profile in [&VIBE, &PI, &OMP, &KIMI, &PRIME_AGENT] {
            assert!(
                profile.parent_meta_namespaces.is_empty(),
                "{}: parent linkage must stay off until observed",
                profile.key
            );
        }
    }

    #[test]
    fn capability_flags_only_set_for_claude_family() {
        for profile in [&CLAUDE, &CLAUDE_CODE] {
            assert!(profile.supports_exit_plan_mode);
            assert!(profile.supports_wakeup_tools);
        }
        // `aoe-agent` sits with the non-Claude group despite bundling Claude
        // as one of its providers: the adapter's tool palette is Read/Write/Bash,
        // with no ExitPlanMode and no ScheduleWakeup to synthesise from (#1904).
        for profile in [
            &CODEX,
            &OPENCODE,
            &GEMINI,
            &VIBE,
            &PI,
            &OMP,
            &KIMI,
            &PRIME_AGENT,
            &AOE_AGENT,
            &DEFAULT,
        ] {
            assert!(!profile.supports_exit_plan_mode, "{}", profile.key);
            assert!(!profile.supports_wakeup_tools, "{}", profile.key);
        }
    }
}
