// Per-agent classifier profiles for the structured view's frontend tool-card
// dispatch. Mirrors src/acp/agent_profiles.rs (server-side gates);
// this side covers the React presentation: which tool names map to
// which cards, which specialised cards (TodoWrite, Skill, Schedule)
// should fire for this agent, which MCP prefixes to recognise.
//
// Profile data is conservative. Where the adapter's actual tool surface
// hasn't been verified hands-on, the entry is omitted rather than
// guessed; the user sees a generic tool card instead of the wrong
// specialised one. Adding a new agent: append an entry below, mirror
// in src/acp/agent_profiles.rs, document in docs/structured-view/multi-agent.md.

/** Card categories the renderer dispatches to. Keep aligned with the
 *  switch in `ToolCards.renderToolCard`. */
export type CardKind = "execute" | "read" | "edit" | "delete" | "search" | "fetch" | "think";

export interface AgentProfile {
  /** Registry key, matches `AgentRegistry` on the server side
   *  (e.g. `claude`, `codex`, `opencode`, `gemini`). */
  key: string;
  /** Capability gates for specialised cards. When a
   *  capability is `false`, the matching classifier short-circuits
   *  before consulting tool title heuristics, so coincidental tool
   *  names on other agents don't render a TodoCard / SkillCard /
   *  ScheduleCard. */
  capabilities: {
    /** Agent todo tools, e.g. Claude `TodoWrite` or OpenCode `todowrite`. */
    todos: boolean;
    /** Claude's `Skill` (kind=other, title=`"Skill"`). */
    skills: boolean;
    /** Claude's `ScheduleWakeup` + cron tools driving `/loop`. */
    wakeup: boolean;
    /** Whether the agent has Claude's notion of a Task subagent.
     *  Currently informational; subagent indentation only renders when
     *  `parentMetaNamespaces` is also non-empty. */
    subagents: boolean;
    /** Whether the mode picker may fall back to Claude's hardcoded
     *  Default/Plan/AcceptEdits/Yolo taxonomy when the agent advertises
     *  no modes through either the config-option channel or ACP
     *  SessionModeState. Claude-family only; other agents render no mode
     *  picker rather than a phantom vocabulary they reject (#1764). */
    legacyModeFallback: boolean;
  };
  /** `_meta.<namespace>.parentToolUseId` lookup order for subagent
   *  child linkage. Empty when the agent's parent-child linkage isn't
   *  verified; the structured view doesn't guess a namespace. */
  parentMetaNamespaces: string[];
  /** MCP tool-name prefixes the structured view recognises. Claude-agent-acp
   *  wraps MCP calls as `mcp__server__verb`; other adapters may use
   *  the same convention or not advertise MCP at all. */
  mcpPrefixes: string[];
  /** Slash-command aliases that reset the conversation. Mirrors
   *  `AgentProfile::clear_aliases` on the Rust side. Used by the
   *  composer's `/` palette to surface clear commands the agent's own
   *  `available_commands_update` channel may not advertise (codex's
   *  `/new`, opencode's `/new`) so the user can discover them via
   *  autocomplete. Each entry should include the leading `/`. */
  clearAliases: string[];
  /** Per-CardKind list of agent-emitted tool names (or titles) that
   *  should route to that card when the wire `tool.kind` lands as
   *  `"other"` or doesn't otherwise indicate the right surface. */
  aliases: Partial<Record<CardKind, string[]>>;
  /** Title equality checks for Claude's specialised cards. The
   *  current claude-agent-acp behavior threads the raw tool name
   *  through the `Other` arm of its title-rewriter; other agents that
   *  happen to emit the same name shouldn't fire the card unless
   *  their profile lists it too. */
  specialTitles: {
    /** Lowercased title values matched for the Skill card. */
    skillNames: string[];
    /** Exact title values matched for the Schedule cards. */
    scheduleNames: string[];
    /** Exact title values matched for the harness-tool cards
     *  (ToolSearch / Monitor / TaskStop). See #2139. */
    harnessNames: string[];
  };
}

const CLAUDE: AgentProfile = {
  key: "claude",
  capabilities: {
    todos: true,
    skills: true,
    wakeup: true,
    subagents: true,
    legacyModeFallback: true,
  },
  parentMetaNamespaces: ["claudeCode"],
  mcpPrefixes: ["mcp__"],
  clearAliases: ["/clear"],
  aliases: {},
  specialTitles: {
    skillNames: ["skill", "claude-skill"],
    scheduleNames: ["ScheduleWakeup", "CronCreate", "CronList", "CronDelete"],
    harnessNames: ["ToolSearch", "Monitor", "TaskStop"],
  },
};

const CLAUDE_CODE: AgentProfile = {
  ...CLAUDE,
  key: "claude-code",
};

const CODEX: AgentProfile = {
  key: "codex",
  capabilities: {
    todos: false,
    skills: false,
    wakeup: false,
    subagents: false,
    legacyModeFallback: false,
  },
  parentMetaNamespaces: [],
  mcpPrefixes: ["mcp__"],
  clearAliases: ["/new"],
  aliases: {
    execute: ["shell", "bash"],
    edit: ["apply_patch"],
    read: ["view_file", "read_file", "read"],
  },
  specialTitles: { skillNames: [], scheduleNames: [], harnessNames: [] },
};

const OPENCODE: AgentProfile = {
  key: "opencode",
  capabilities: {
    todos: true,
    skills: false,
    wakeup: false,
    subagents: true,
    legacyModeFallback: false,
  },
  parentMetaNamespaces: [],
  mcpPrefixes: ["mcp__"],
  clearAliases: ["/new"],
  aliases: {
    execute: ["bash"],
    read: ["read"],
    edit: ["edit", "write"],
    search: ["grep", "glob"],
    fetch: ["webfetch"],
    think: ["task"],
  },
  specialTitles: { skillNames: [], scheduleNames: [], harnessNames: [] },
};

const GEMINI: AgentProfile = {
  key: "gemini",
  capabilities: {
    todos: false,
    skills: false,
    wakeup: false,
    subagents: false,
    legacyModeFallback: false,
  },
  parentMetaNamespaces: [],
  mcpPrefixes: ["mcp__"],
  clearAliases: [],
  aliases: {
    execute: ["run_shell_command"],
    read: ["read_file", "read_many_files"],
    edit: ["write_file", "edit"],
    search: ["grep", "glob"],
    fetch: ["web_fetch"],
  },
  specialTitles: { skillNames: [], scheduleNames: [], harnessNames: [] },
};

const VIBE: AgentProfile = {
  key: "vibe",
  capabilities: {
    todos: false,
    skills: false,
    wakeup: false,
    subagents: false,
    legacyModeFallback: false,
  },
  parentMetaNamespaces: [],
  mcpPrefixes: ["mcp__"],
  clearAliases: [],
  aliases: {},
  specialTitles: { skillNames: [], scheduleNames: [], harnessNames: [] },
};

const PI: AgentProfile = {
  key: "pi",
  capabilities: {
    todos: false,
    skills: false,
    wakeup: false,
    subagents: false,
    legacyModeFallback: false,
  },
  parentMetaNamespaces: [],
  mcpPrefixes: ["mcp__"],
  clearAliases: [],
  aliases: {},
  specialTitles: { skillNames: [], scheduleNames: [], harnessNames: [] },
};

const AOE_AGENT: AgentProfile = {
  ...CLAUDE,
  key: "aoe-agent",
};

/** Permissive fallback for unknown agent keys: kind-only dispatch with
 *  no claude-specific specials. Keeps custom or future agents working
 *  through the generic card path rather than crashing. */
export const DEFAULT_AGENT_PROFILE: AgentProfile = {
  key: "default",
  capabilities: {
    todos: false,
    skills: false,
    wakeup: false,
    subagents: false,
    legacyModeFallback: false,
  },
  parentMetaNamespaces: [],
  mcpPrefixes: ["mcp__"],
  clearAliases: [],
  aliases: {},
  specialTitles: { skillNames: [], scheduleNames: [], harnessNames: [] },
};

const PROFILES: Record<string, AgentProfile> = {
  claude: CLAUDE,
  "claude-code": CLAUDE_CODE,
  codex: CODEX,
  opencode: OPENCODE,
  gemini: GEMINI,
  vibe: VIBE,
  pi: PI,
  "aoe-agent": AOE_AGENT,
};

/** Resolve a profile by the session's `tool` key. Unknown keys (and
 *  `null` / `undefined`) fall back to `DEFAULT_AGENT_PROFILE`. */
export function resolveAgentProfile(toolKey: string | null | undefined): AgentProfile {
  if (!toolKey) return DEFAULT_AGENT_PROFILE;
  return PROFILES[toolKey] ?? DEFAULT_AGENT_PROFILE;
}

/** True when `text`'s trimmed body matches one of `aliases`, either as
 *  the entire prompt or as a `<alias> <args>` invocation. Mirror of the
 *  server-side `AgentProfile::is_clear_command` in
 *  `src/acp/agent_profiles.rs` so the structured view's combined-mode drain
 *  splits at exactly the same boundary the server detects as a session
 *  clear (#1356). */
export function isClearAlias(text: string, aliases: ReadonlyArray<string>): boolean {
  const trimmed = text.trim();
  if (trimmed.length === 0) return false;
  for (const alias of aliases) {
    if (trimmed === alias) return true;
    if (trimmed.startsWith(alias)) {
      const rest = trimmed.slice(alias.length);
      if (rest.length > 0 && /^\s/.test(rest)) return true;
    }
  }
  return false;
}
