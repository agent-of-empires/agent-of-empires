//! Server-side per-agent capability and naming profiles.

use std::collections::BTreeMap;

use crate::json_probe::JsonProbe;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Tool names routed to each tool card when `tool.kind` does not indicate it.
#[derive(Debug, Clone, Copy)]
pub struct CardAliases {
    pub execute: &'static [&'static str],
    pub read: &'static [&'static str],
    pub edit: &'static [&'static str],
    pub delete: &'static [&'static str],
    pub search: &'static [&'static str],
    pub fetch: &'static [&'static str],
    pub think: &'static [&'static str],
}

impl CardAliases {
    pub const NONE: CardAliases = CardAliases {
        execute: &[],
        read: &[],
        edit: &[],
        delete: &[],
        search: &[],
        fetch: &[],
        think: &[],
    };
}

/// Exact titles that select the specialised cards.
#[derive(Debug, Clone, Copy)]
pub struct SpecialTitles {
    pub skill_names: &'static [&'static str],
    pub schedule_names: &'static [&'static str],
    pub harness_names: &'static [&'static str],
    pub workflow_names: &'static [&'static str],
}

impl SpecialTitles {
    pub const NONE: SpecialTitles = SpecialTitles {
        skill_names: &[],
        schedule_names: &[],
        harness_names: &[],
        workflow_names: &[],
    };
}

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
    /// Slash commands that compact the conversation.
    pub compact_aliases: &'static [&'static str],
    pub clear_requires_driven_reset: bool,
    /// When true, a `switch_mode` approval carries its `raw_input.plan`
    /// markdown for the plan viewer (Claude's ExitPlanMode shape).
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
    /// Native agent whose on-disk configuration carries conversation state.
    pub native_config_agent: Option<&'static str>,
    /// When true, the agent implements the ACP subagents RFD, so aoe declares
    /// the client capability and each subagent arrives as a child session.
    pub supports_subagents: bool,
    /// The agent expands `${VAR}` in MCP entries from its own environment, so aoe can pass
    /// plugin credentials as references and keep the values off the command line. Verified
    /// for `claude-agent-acp` only; other agents get no plugin entries.
    pub expands_mcp_env_refs: bool,
    /// The agent's todo tool is trusted, so title heuristics may render a todo card.
    pub todos: bool,
    /// The agent has a Skill tool, so title heuristics may render a skill card.
    pub skills: bool,
    /// May fall back to Claude's mode taxonomy when the agent advertises no modes.
    pub legacy_mode_fallback: bool,
    /// `raw_name`s that launch an off-protocol subagent, matched case-sensitively.
    pub subagent_tool_names: &'static [&'static str],
    pub mcp_prefixes: &'static [&'static str],
    pub card_aliases: CardAliases,
    pub special_titles: SpecialTitles,
    /// Regex a tool path must match to be a memory file op.
    pub memory_path_regex: Option<&'static str>,
    /// The mobile terminal shows only the Monitor view for this agent.
    pub mobile_monitor_only: bool,
    /// Daemon-side probe of the agent's fullscreen renderer setting. Never sent to clients.
    pub fullscreen_probe: Option<JsonProbe>,
    /// Case-insensitive regex the one-shot capture must match before banner lines are stripped.
    pub oneshot_gate: Option<&'static str>,
    /// Case-insensitive line regexes of the startup banner dropped from a one-shot capture.
    pub oneshot_strip_patterns: &'static [&'static str],
}

fn matches_alias(aliases: &[&str], text: &str) -> bool {
    let trimmed = text.trim();
    aliases.iter().any(|alias| {
        trimmed == *alias
            || trimmed
                .strip_prefix(alias)
                .is_some_and(|rest| rest.starts_with(char::is_whitespace))
    })
}

impl AgentProfile {
    /// True when `text` matches any of this profile's clear-conversation
    /// slash aliases, tolerating surrounding whitespace and a trailing
    /// argument cluster.
    pub fn is_clear_command(&self, text: &str) -> bool {
        matches_alias(self.clear_aliases, text)
    }

    /// Same matching for the compact-conversation aliases.
    pub fn is_compact_command(&self, text: &str) -> bool {
        matches_alias(self.compact_aliases, text)
    }

    /// Commands that must run as their own turn, never steered into a
    /// running one or batched with neighbouring prompts.
    pub fn is_solo_command(&self, text: &str) -> bool {
        self.is_clear_command(text) || self.is_compact_command(text)
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

    /// Evaluate the fullscreen probe through `memo`, false without one.
    pub fn probe_fullscreen(&self, memo: &crate::json_probe::Memo) -> bool {
        self.fullscreen_probe
            .as_ref()
            .is_some_and(|probe| memo.matches(probe))
    }

    /// True iff the agent surfaces session-start memory recall through
    /// the tool channel with the `_meta.claudeCode.toolName` namespace
    /// claude-agent-acp adopted in v0.37.0 (upstream).
    pub fn supports_memory_recall_tool(&self) -> bool {
        self.parent_meta_namespaces.contains(&"claudeCode")
    }
}

/// Current `AgentProfileWire` shape version. Bump when a field is renamed, removed or retyped.
pub const WIRE_VERSION: u32 = 1;

/// `SpecialTitles` as sent to clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SpecialTitlesWire {
    pub skill_names: Vec<String>,
    pub schedule_names: Vec<String>,
    pub harness_names: Vec<String>,
    pub workflow_names: Vec<String>,
}

/// The client-facing view of an [`AgentProfile`], sent on each session as `agent_profile`.
/// The fullscreen probe declaration, lifecycle and one-shot banner patterns are not part of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct AgentProfileWire {
    /// Shape version, see [`WIRE_VERSION`].
    pub v: u32,
    pub key: String,
    pub parent_meta_namespaces: Vec<String>,
    pub clear_aliases: Vec<String>,
    pub compact_aliases: Vec<String>,
    pub clear_requires_driven_reset: bool,
    pub supports_exit_plan_mode: bool,
    pub supports_wakeup_tools: bool,
    pub emits_heartbeat_keepalives: bool,
    pub yolo_mode_id: Option<String>,
    pub supports_subagents: bool,
    pub todos: bool,
    pub skills: bool,
    pub legacy_mode_fallback: bool,
    pub subagent_tool_names: Vec<String>,
    pub mcp_prefixes: Vec<String>,
    /// Tool names per card kind; kinds without aliases are absent.
    pub card_aliases: BTreeMap<String, Vec<String>>,
    pub special_titles: SpecialTitlesWire,
    pub memory_path_regex: Option<String>,
    pub mobile_monitor_only: bool,
    /// Result of the profile's fullscreen probe, evaluated by the daemon.
    pub fullscreen: bool,
}

fn owned(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

impl AgentProfileWire {
    pub fn new(profile: &AgentProfile, fullscreen: bool) -> Self {
        let cards = &profile.card_aliases;
        let titles = &profile.special_titles;
        Self {
            v: WIRE_VERSION,
            key: profile.key.to_string(),
            parent_meta_namespaces: owned(profile.parent_meta_namespaces),
            clear_aliases: owned(profile.clear_aliases),
            compact_aliases: owned(profile.compact_aliases),
            clear_requires_driven_reset: profile.clear_requires_driven_reset,
            supports_exit_plan_mode: profile.supports_exit_plan_mode,
            supports_wakeup_tools: profile.supports_wakeup_tools,
            emits_heartbeat_keepalives: profile.emits_heartbeat_keepalives,
            yolo_mode_id: profile.yolo_mode_id.map(str::to_string),
            supports_subagents: profile.supports_subagents,
            todos: profile.todos,
            skills: profile.skills,
            legacy_mode_fallback: profile.legacy_mode_fallback,
            subagent_tool_names: owned(profile.subagent_tool_names),
            mcp_prefixes: owned(profile.mcp_prefixes),
            card_aliases: [
                ("execute", cards.execute),
                ("read", cards.read),
                ("edit", cards.edit),
                ("delete", cards.delete),
                ("search", cards.search),
                ("fetch", cards.fetch),
                ("think", cards.think),
            ]
            .into_iter()
            .filter(|(_, names)| !names.is_empty())
            .map(|(kind, names)| (kind.to_string(), owned(names)))
            .collect(),
            special_titles: SpecialTitlesWire {
                skill_names: owned(titles.skill_names),
                schedule_names: owned(titles.schedule_names),
                harness_names: owned(titles.harness_names),
                workflow_names: owned(titles.workflow_names),
            },
            memory_path_regex: profile.memory_path_regex.map(str::to_string),
            mobile_monitor_only: profile.mobile_monitor_only,
            fullscreen,
        }
    }
}

impl Default for AgentProfileWire {
    fn default() -> Self {
        Self::new(&DEFAULT, false)
    }
}

/// Permissive base for unknown registry keys: no claude-specific gates fire,
/// no clear aliases match, no parent-meta lookup. Every profile below states
/// only what it changes.
pub const DEFAULT: AgentProfile = AgentProfile {
    key: "default",
    parent_meta_namespaces: &[],
    clear_aliases: &[],
    compact_aliases: &[],
    clear_requires_driven_reset: false,
    supports_exit_plan_mode: false,
    supports_wakeup_tools: false,
    emits_heartbeat_keepalives: false,
    yolo_mode_id: None,
    native_config_agent: None,
    supports_subagents: false,
    expands_mcp_env_refs: false,
    todos: false,
    skills: false,
    legacy_mode_fallback: false,
    subagent_tool_names: &[],
    mcp_prefixes: &["mcp__"],
    card_aliases: CardAliases::NONE,
    special_titles: SpecialTitles::NONE,
    memory_path_regex: None,
    mobile_monitor_only: false,
    fullscreen_probe: None,
    oneshot_gate: None,
    oneshot_strip_patterns: &[],
};

/// Claude via `claude-agent-acp`.
pub const CLAUDE: AgentProfile = AgentProfile {
    key: "claude",
    parent_meta_namespaces: &["claudeCode"],
    clear_aliases: &["/clear"],
    compact_aliases: &["/compact"],
    clear_requires_driven_reset: true,
    supports_exit_plan_mode: true,
    supports_wakeup_tools: true,
    emits_heartbeat_keepalives: true,
    yolo_mode_id: Some("bypassPermissions"),
    native_config_agent: Some("claude"),
    supports_subagents: true,
    expands_mcp_env_refs: true,
    todos: true,
    skills: true,
    legacy_mode_fallback: true,
    special_titles: SpecialTitles {
        skill_names: &["skill", "claude-skill"],
        schedule_names: &["ScheduleWakeup", "CronCreate", "CronList", "CronDelete"],
        harness_names: &["ToolSearch", "Monitor", "TaskStop", "SendMessage"],
        workflow_names: &["Workflow"],
    },
    memory_path_regex: Some(r"/\.claude/projects/[^/]+/memory/.*\.md$"),
    mobile_monitor_only: true,
    fullscreen_probe: Some(JsonProbe {
        path: ".claude/settings.json",
        pointer: "/tui",
        equals: "fullscreen",
    }),
    oneshot_gate: Some("claude code"),
    oneshot_strip_patterns: CLAUDE_BANNER_PATTERNS,
    ..DEFAULT
};

/// Line patterns of the Claude Code startup banner: a box whose borders and
/// edges are box-drawing glyphs and whose logo is block elements, the notices
/// printed under it, and numbered tips.
const CLAUDE_BANNER_PATTERNS: &[&str] = &[
    // Blank, or only box-drawing (U+2500..=U+257F) and block-element (U+2580..=U+259F) glyphs.
    r"^[\s\x{2500}-\x{259F}]*$",
    r"^\s*[\x{2500}-\x{259F}|]",
    r"claude code v|welcome to claude code|welcome back|tips for getting started|what's new|/release-notes|run /mcp|need authentication|/help for help",
    // Notice and tip callouts: U+26A0, U+203B, "tip:".
    r"^\s*(?:[\x{26A0}\x{203B}]|tip:)",
    // Numbered tip: "1." or "2)".
    r"^\s*[0-9][.)]",
];

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
    native_config_agent: Some("codex"),
    supports_subagents: true,
    card_aliases: CardAliases {
        execute: &["shell", "bash"],
        edit: &["apply_patch"],
        read: &["view_file", "read_file", "read"],
        ..CardAliases::NONE
    },
    ..DEFAULT
};

/// SST OpenCode via native `opencode acp`. Its bypass-mode id over ACP is
/// unverified, so YOLO stays a no-op rather than guessing an id.
pub const OPENCODE: AgentProfile = AgentProfile {
    key: "opencode",
    clear_aliases: &["/new"],
    native_config_agent: Some("opencode"),
    todos: true,
    subagent_tool_names: &["task"],
    card_aliases: CardAliases {
        execute: &["bash"],
        read: &["read"],
        edit: &["edit", "write"],
        search: &["grep", "glob"],
        fetch: &["webfetch"],
        ..CardAliases::NONE
    },
    ..DEFAULT
};

/// Google Gemini CLI via native `gemini --acp`. It surfaces its YOLO approval
/// mode with the `yolo` id (see `acp_client/update_events.rs`).
pub const GEMINI: AgentProfile = AgentProfile {
    key: "gemini",
    yolo_mode_id: Some("yolo"),
    native_config_agent: Some("gemini"),
    card_aliases: CardAliases {
        execute: &["run_shell_command"],
        read: &["read_file", "read_many_files"],
        edit: &["write_file", "edit"],
        search: &["grep", "glob"],
        fetch: &["web_fetch"],
        ..CardAliases::NONE
    },
    ..DEFAULT
};

/// Mistral Vibe via bundled `vibe-acp`.
pub const VIBE: AgentProfile = AgentProfile {
    key: "vibe",
    native_config_agent: Some("vibe"),
    ..DEFAULT
};

/// Pi coding agent via `pi-acp`.
pub const PI: AgentProfile = AgentProfile {
    key: "pi",
    native_config_agent: Some("pi"),
    ..DEFAULT
};

/// Oh My Pi via native `omp acp`.
pub const OMP: AgentProfile = AgentProfile {
    key: "omp",
    clear_aliases: &["/new"],
    native_config_agent: Some("omp"),
    ..DEFAULT
};

/// Kimi Code (Moonshot AI) via native `kimi acp`.
pub const KIMI: AgentProfile = AgentProfile {
    key: "kimi",
    clear_aliases: &["/new"],
    yolo_mode_id: Some("yolo"),
    native_config_agent: Some("kimi"),
    ..DEFAULT
};

/// PrimeIntellect Prime Agent via native `prime-agent --mode acp`.
pub const PRIME_AGENT: AgentProfile = AgentProfile {
    key: "prime-agent",
    native_config_agent: Some("prime-agent"),
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

/// True when `text` could be a declared reset command (`/clear` or `/new`). A prompt that is
/// neither never needs the session configuration to be read to classify it.
pub fn could_be_declared_alias(text: &str) -> bool {
    matches_alias(&["/clear", "/new"], text)
}

/// True when `key` names a built-in agent: one in the agent registry or with its own profile.
/// A built-in agent keeps its own reset command and ignores `session.agent_clear_alias`.
pub fn is_builtin(key: &str) -> bool {
    crate::agents::get_agent(key).is_some() || resolve(key).key != DEFAULT.key
}

/// A custom agent that resets with `/clear`, declared by `session.agent_clear_alias`.
pub const CUSTOM_CLEAR: AgentProfile = AgentProfile {
    key: "custom-clear",
    clear_aliases: &["/clear"],
    clear_requires_driven_reset: true,
    ..DEFAULT
};

/// A custom agent that resets with `/new`, declared by `session.agent_clear_alias`.
pub const CUSTOM_NEW: AgentProfile = AgentProfile {
    key: "custom-new",
    clear_aliases: &["/new"],
    clear_requires_driven_reset: true,
    ..DEFAULT
};

/// The profile a declared reset alias selects, or `None` for anything but `/clear` or `/new`.
pub fn alias_profile(alias: &str) -> Option<&'static AgentProfile> {
    match alias.trim() {
        "/clear" => Some(&CUSTOM_CLEAR),
        "/new" => Some(&CUSTOM_NEW),
        _ => None,
    }
}

/// [`resolve`], except that a custom agent takes the reset alias `session` declares for it.
pub fn resolve_with_config(
    key: &str,
    session: &crate::session::config::SessionConfig,
) -> &'static AgentProfile {
    session.clear_alias_profile(key).unwrap_or(resolve(key))
}

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
    fn a_declared_alias_gives_a_custom_agent_a_reset_but_never_a_built_in() {
        let mut session = crate::session::config::SessionConfig::default();
        // `cursor` is a registry agent with no ACP profile of its own.
        for (agent, alias) in [
            ("my-claude", "/clear"),
            ("my-codex", " /new "),
            ("odd", "/compact"),
            ("claude", "/new"),
            ("cursor", "/clear"),
        ] {
            session.agent_clear_alias.insert(agent.into(), alias.into());
            session
                .agent_acp_cmd
                .insert(agent.into(), format!("{agent}-bridge acp"));
        }
        // (agent, text, resets)
        let cases = [
            ("my-claude", "/clear", true),
            ("my-claude", "/new", false),
            ("my-codex", "/new", true),
            ("my-codex", "/clear", false),
            ("odd", "/compact", false),
            ("unlisted", "/clear", false),
            ("claude", "/clear", true),
            ("claude", "/new", false),
            ("cursor", "/clear", false),
        ];
        for (agent, text, resets) in cases {
            let profile = resolve_with_config(agent, &session);
            assert_eq!(
                profile.is_clear_command(text) && profile.clear_requires_driven_reset,
                resets,
                "{agent} {text:?}"
            );
        }
    }

    #[test]
    fn only_a_clear_or_new_command_can_match_a_declared_alias() {
        // (text, could match)
        let cases = [
            ("/clear", true),
            ("  /new  ", true),
            ("/clear --hard", true),
            ("/compact", false),
            ("clear the cache", false),
            ("please /clear", false),
            ("", false),
        ];
        for (text, expected) in cases {
            assert_eq!(could_be_declared_alias(text), expected, "{text:?}");
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

    #[test]
    fn agent_profile_wire_shape_is_versioned() {
        const KEYS: [&str; 21] = [
            "card_aliases",
            "clear_aliases",
            "clear_requires_driven_reset",
            "compact_aliases",
            "emits_heartbeat_keepalives",
            "fullscreen",
            "key",
            "legacy_mode_fallback",
            "mcp_prefixes",
            "memory_path_regex",
            "mobile_monitor_only",
            "parent_meta_namespaces",
            "skills",
            "special_titles",
            "subagent_tool_names",
            "supports_exit_plan_mode",
            "supports_subagents",
            "supports_wakeup_tools",
            "todos",
            "v",
            "yolo_mode_id",
        ];
        let mut session = crate::session::config::SessionConfig::default();
        session
            .agent_clear_alias
            .insert("my-agent".into(), "/new".into());
        session
            .agent_acp_cmd
            .insert("my-agent".into(), "my-agent-bridge acp".into());
        for key in [
            "claude",
            "claude-code",
            "codex",
            "opencode",
            "gemini",
            "unknown",
            "my-agent",
        ] {
            let profile = resolve_with_config(key, &session);
            let value = serde_json::to_value(AgentProfileWire::new(profile, false)).unwrap();
            let mut keys: Vec<&str> = value
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort_unstable();
            // Changing the shape needs a WIRE_VERSION bump and a new `v` here.
            assert_eq!(value["v"], 1, "{key}");
            assert_eq!(keys, KEYS, "{key}");
        }
        let claude = serde_json::to_value(AgentProfileWire::new(&CLAUDE, true)).unwrap();
        assert_eq!(claude["clear_aliases"], serde_json::json!(["/clear"]));
        assert_eq!(claude["fullscreen"], true);
        assert!(claude["memory_path_regex"].is_string());
        let custom = resolve_with_config("my-agent", &session);
        assert_eq!(custom.clear_aliases, ["/new"]);
    }
}
