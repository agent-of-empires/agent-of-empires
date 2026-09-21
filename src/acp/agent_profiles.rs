//! Server-side per-agent capability and naming profiles.

/// Per-agent server-side profile.
#[derive(Debug, Clone)]
pub struct AgentProfile {
    /// Registry key.
    pub key: &'static str,
    /// `_meta.<namespace>.parentToolUseId` lookup order for subagent
    /// linkage.
    pub parent_meta_namespaces: &'static [&'static str],
    /// Slash commands that reset the conversation.
    pub clear_aliases: &'static [&'static str],
    pub clear_requires_driven_reset: bool,
    /// When true, the server synthesises a `PlanUpdated` event from a
    /// `kind: switch_mode` tool call (Claude's ExitPlanMode shape).
    pub supports_exit_plan_mode: bool,
    /// When true, the server synthesises a `WakeupScheduled` event from
    /// a tool call titled `"ScheduleWakeup"`.
    pub supports_wakeup_tools: bool,
    /// When true, the agent emits keepalive progress pings for
    /// long-running tools under a derived id `<baseToolId>-heartbeat-<N>`
    /// (see `acp_client::is_heartbeat_tool_call_id`).
    pub emits_heartbeat_keepalives: bool,
    /// ACP session-mode id that means "bypass all permission prompts"
    /// (the wizard's "Auto-approve" / profile `yolo_mode_default`).
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
    /// namespace this profile knows about.
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
    /// claude-agent-acp adopted in v0.37.0 (upstream).
    pub fn supports_memory_recall_tool(&self) -> bool {
        self.parent_meta_namespaces.contains(&"claudeCode")
    }
}

/// Permissive base for unknown registry keys: no claude-specific gates fire,
/// no clear aliases match, no parent-meta lookup. Every profile below states
/// only what it changes.
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

/// Claude via `claude-agent-acp`.
pub const CLAUDE: AgentProfile = AgentProfile {
    key: "claude",
    parent_meta_namespaces: &["claudeCode"],
    clear_aliases: &["/clear"],
    clear_requires_driven_reset: true,
    supports_exit_plan_mode: true,
    supports_wakeup_tools: true,
    emits_heartbeat_keepalives: true,
    yolo_mode_id: Some("bypassPermissions"),
};

/// Legacy alias key carried by older session records (`agent_name="claude-code"`).
pub const CLAUDE_CODE: AgentProfile = AgentProfile {
    key: "claude-code",
    ..CLAUDE
};

/// OpenAI Codex CLI via `@agentclientprotocol/codex-acp`.
pub const CODEX: AgentProfile = AgentProfile {
    key: "codex",
    clear_aliases: &["/new"],
    clear_requires_driven_reset: true,
    yolo_mode_id: Some("agent-full-access"),
    ..DEFAULT
};

/// SST OpenCode via native `opencode acp`. Its bypass-mode id over ACP is
/// unverified, so YOLO stays a no-op rather than guessing an id.
pub const OPENCODE: AgentProfile = AgentProfile {
    key: "opencode",
    clear_aliases: &["/new"],
    ..DEFAULT
};

/// Google Gemini CLI via native `gemini --acp`. It surfaces its YOLO approval
/// mode with the `yolo` id (see `acp_client/update_events.rs`).
pub const GEMINI: AgentProfile = AgentProfile {
    key: "gemini",
    yolo_mode_id: Some("yolo"),
    ..DEFAULT
};

/// Mistral Vibe via bundled `vibe-acp`.
pub const VIBE: AgentProfile = AgentProfile {
    key: "vibe",
    ..DEFAULT
};

/// Pi coding agent via `pi-acp`.
pub const PI: AgentProfile = AgentProfile {
    key: "pi",
    ..DEFAULT
};

/// Oh My Pi via native `omp acp`.
pub const OMP: AgentProfile = AgentProfile {
    key: "omp",
    clear_aliases: &["/new"],
    ..DEFAULT
};

/// Kimi Code (Moonshot AI) via native `kimi acp`.
pub const KIMI: AgentProfile = AgentProfile {
    key: "kimi",
    clear_aliases: &["/new"],
    yolo_mode_id: Some("yolo"),
    ..DEFAULT
};

/// PrimeIntellect Prime Agent via native `prime-agent --mode acp`.
pub const PRIME_AGENT: AgentProfile = AgentProfile {
    key: "prime-agent",
    ..DEFAULT
};

/// aoe's own multi-provider agent (Vercel AI SDK 7), at
/// `acp-worker/aoe-agent/src/index.ts`. A forwarded /clear is ordinary model
/// text, and `session/set_mode` accepts any id while changing nothing.
pub const AOE_AGENT: AgentProfile = AgentProfile {
    key: "aoe-agent",
    clear_aliases: &["/clear"],
    clear_requires_driven_reset: true,
    ..DEFAULT
};

/// Resolve a static profile by registry key.
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

pub fn is_reviewed(key: &str) -> bool {
    matches!(
        key,
        "claude" | "claude-code" | "codex" | "gemini" | "kimi" | "aoe-agent"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One row per registry key: the adapter conventions the server gates on.
    /// Unverified adapters stay off until their behavior is observed.
    #[test]
    fn profile_matrix_is_pinned_per_registry_key() {
        // (key, clear aliases, driven reset, parent namespaces, claude-family
        //  tool gates, yolo mode id, reviewed)
        type Case<'a> = (
            &'a str,
            &'a [&'a str],
            bool,
            &'a [&'a str],
            bool,
            Option<&'a str>,
            bool,
        );
        let cases: [Case; 13] = [
            (
                "claude",
                &["/clear"],
                true,
                &["claudeCode"],
                true,
                Some("bypassPermissions"),
                true,
            ),
            (
                "claude-code",
                &["/clear"],
                true,
                &["claudeCode"],
                true,
                Some("bypassPermissions"),
                true,
            ),
            (
                "codex",
                &["/new"],
                true,
                &[],
                false,
                Some("agent-full-access"),
                true,
            ),
            ("opencode", &["/new"], false, &[], false, None, false),
            ("gemini", &[], false, &[], false, Some("yolo"), true),
            ("vibe", &[], false, &[], false, None, false),
            ("pi", &[], false, &[], false, None, false),
            ("omp", &["/new"], false, &[], false, None, false),
            ("kimi", &["/new"], false, &[], false, Some("yolo"), true),
            ("prime-agent", &[], false, &[], false, None, false),
            ("aoe-agent", &["/clear"], true, &[], false, None, true),
            ("unknown-agent", &[], false, &[], false, None, false),
            ("", &[], false, &[], false, None, false),
        ];
        for (key, aliases, driven, namespaces, claude_gates, yolo, reviewed) in cases {
            let p = resolve(key);
            let want_key = if p.key == "default" { "default" } else { key };
            assert_eq!(p.key, want_key, "{key}");
            assert_eq!(p.clear_aliases, aliases, "{key}");
            assert_eq!(p.clear_requires_driven_reset, driven, "{key}");
            assert_eq!(p.parent_meta_namespaces, namespaces, "{key}");
            assert_eq!(p.supports_exit_plan_mode, claude_gates, "{key}");
            assert_eq!(p.supports_wakeup_tools, claude_gates, "{key}");
            assert_eq!(p.emits_heartbeat_keepalives, claude_gates, "{key}");
            assert_eq!(p.supports_memory_recall_tool(), claude_gates, "{key}");
            assert_eq!(p.yolo_mode_id, yolo, "{key}");
            assert_eq!(is_reviewed(key), reviewed, "{key}");
        }
    }

    #[test]
    fn is_clear_command_matches_an_alias_and_its_argument_cluster() {
        // (profile, text, matches)
        let cases: [(&AgentProfile, &str, bool); 13] = [
            (&CLAUDE, "/clear", true),
            (&CLAUDE, "  /clear  ", true),
            (&CLAUDE, "/clear --hard", true),
            (&CLAUDE, "/new", false),
            (&CLAUDE, "clear", false),
            (&CLAUDE, "/cleart", false),
            (&CLAUDE, "hello /clear world", false),
            (&CLAUDE, "", false),
            (&CODEX, "/new", true),
            (&CODEX, "/clear", false),
            (&OMP, "/new", true),
            // gemini has no clear alias, so nothing matches.
            (&GEMINI, "/clear", false),
            (&GEMINI, "/new", false),
        ];
        for (profile, text, want) in cases {
            assert_eq!(
                profile.is_clear_command(text),
                want,
                "{} {text:?}",
                profile.key
            );
        }
    }

    #[test]
    fn parent_tool_use_id_reads_only_a_profiles_own_namespace() {
        let meta = |namespace: &str, value: serde_json::Value| {
            let mut map = serde_json::Map::new();
            map.insert(
                namespace.to_string(),
                serde_json::json!({ "parentToolUseId": value }),
            );
            Some(map)
        };
        assert_eq!(
            CLAUDE.parent_tool_use_id_from_meta(&meta("claudeCode", "tc-parent-7".into())),
            Some("tc-parent-7".to_string())
        );
        // (profile, meta) pairs that must not resolve a parent.
        let misses = [
            (&CLAUDE, meta("otherNamespace", "tc-x".into())),
            (&CLAUDE, meta("claudeCode", 42.into())),
            (&CLAUDE, None),
            (&OPENCODE, meta("opencode", "tc-9".into())),
            (&AOE_AGENT, meta("claudeCode", "tc-parent-7".into())),
        ];
        for (profile, meta) in misses {
            assert!(
                profile.parent_tool_use_id_from_meta(&meta).is_none(),
                "{}",
                profile.key
            );
        }
    }
}
