// Per-agent tool-card classifier profiles; the React side of src/acp/agent_profiles.rs.

/** Keep aligned with `ToolCards.renderToolCard`. */
export type CardKind = "execute" | "read" | "edit" | "delete" | "search" | "fetch" | "think";

export interface AgentProfile {
  key: string;
  /** When false, the matching classifier skips title heuristics so coincidental tool names don't render special cards. */
  capabilities: {
    todos: boolean;
    skills: boolean;
    wakeup: boolean;
    subagents: boolean;
    /** May fall back to Claude's hardcoded mode taxonomy when the agent advertises none. */
    legacyModeFallback: boolean;
    /** Heartbeat frames under `<baseToolId>-heartbeat-<N>` are ignored only for these agents. Mirrors `emits_heartbeat_keepalives`. */
    heartbeatKeepalives: boolean;
  };
  /** `_meta.<namespace>.parentToolUseId` lookup order; empty when linkage is unverified. */
  parentMetaNamespaces: string[];
  /** `raw_name`s that launch an off-protocol subagent (opencode `task`), matched case-sensitively. */
  subagentToolNames: string[];
  mcpPrefixes: string[];
  /** Tool names routed to a card when `tool.kind` doesn't indicate it. */
  aliases: Partial<Record<CardKind, string[]>>;
  /** Exact titles for the specialised cards; other agents don't fire them unless listed. */
  specialTitles: {
    /** Lowercased title values matched for the Skill card. */
    skillNames: string[];
    scheduleNames: string[];
    harnessNames: string[];
  };
  /** Mirrors `AgentDef.lifecycle` in src/agents.rs; absent for Active agents. */
  lifecycle?: AgentLifecycleInfo;
}

const CLAUDE: AgentProfile = {
  key: "claude",
  // Claude's subagent parent is linked via parentMetaNamespaces, not by name.
  subagentToolNames: [],
  capabilities: {
    todos: true,
    skills: true,
    wakeup: true,
    subagents: true,
    legacyModeFallback: true,
    heartbeatKeepalives: true,
  },
  parentMetaNamespaces: ["claudeCode"],
  mcpPrefixes: ["mcp__"],
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
  subagentToolNames: [],
  capabilities: {
    todos: false,
    skills: false,
    wakeup: false,
    subagents: false,
    legacyModeFallback: false,
    heartbeatKeepalives: false,
  },
  parentMetaNamespaces: [],
  mcpPrefixes: ["mcp__"],
  aliases: {
    execute: ["shell", "bash"],
    edit: ["apply_patch"],
    read: ["view_file", "read_file", "read"],
  },
  specialTitles: { skillNames: [], scheduleNames: [], harnessNames: [] },
};

const OPENCODE: AgentProfile = {
  key: "opencode",
  // opencode's `task` streams no children, only a final <task_result>.
  subagentToolNames: ["task"],
  capabilities: {
    todos: true,
    skills: false,
    wakeup: false,
    subagents: true,
    legacyModeFallback: false,
    heartbeatKeepalives: false,
  },
  parentMetaNamespaces: [],
  mcpPrefixes: ["mcp__"],
  aliases: {
    execute: ["bash"],
    read: ["read"],
    edit: ["edit", "write"],
    search: ["grep", "glob"],
    fetch: ["webfetch"],
  },
  specialTitles: { skillNames: [], scheduleNames: [], harnessNames: [] },
};

const GEMINI: AgentProfile = {
  key: "gemini",
  subagentToolNames: [],
  capabilities: {
    todos: false,
    skills: false,
    wakeup: false,
    subagents: false,
    legacyModeFallback: false,
    heartbeatKeepalives: false,
  },
  parentMetaNamespaces: [],
  mcpPrefixes: ["mcp__"],
  aliases: {
    execute: ["run_shell_command"],
    read: ["read_file", "read_many_files"],
    edit: ["write_file", "edit"],
    search: ["grep", "glob"],
    fetch: ["web_fetch"],
  },
  specialTitles: { skillNames: [], scheduleNames: [], harnessNames: [] },
  // Mirrors AgentLifecycle::Deprecated in src/agents.rs: Google cut off consumer accounts.
  lifecycle: {
    state: "deprecated",
    since: "2026-06-18",
    note: "consumer accounts cut off by Google; enterprise/API-key remain valid",
    replacement: "antigravity",
  },
};

/** Mirrors `AgentLifecycle` in src/agents.rs; lives here so this module stays dependency-free. */
export type AgentLifecycleInfo =
  | { state: "active" }
  | {
      state: "deprecated";
      since: string;
      note: string;
      replacement: string | null;
    };

export const ACTIVE_LIFECYCLE: AgentLifecycleInfo = { state: "active" };

export function resolveAgentLifecycle(toolKey: string | null | undefined): AgentLifecycleInfo {
  if (!toolKey) return ACTIVE_LIFECYCLE;
  return PROFILES[toolKey]?.lifecycle ?? ACTIVE_LIFECYCLE;
}

/** Server-reported lifecycle wins; the static mirror covers older daemons and unlisted keys. */
export function effectiveLifecycle(
  source: { lifecycle?: AgentLifecycleInfo } | undefined,
  name: string | null | undefined,
): AgentLifecycleInfo {
  return source?.lifecycle ?? resolveAgentLifecycle(name);
}

const VIBE: AgentProfile = {
  key: "vibe",
  subagentToolNames: [],
  capabilities: {
    todos: false,
    skills: false,
    wakeup: false,
    subagents: false,
    legacyModeFallback: false,
    heartbeatKeepalives: false,
  },
  parentMetaNamespaces: [],
  mcpPrefixes: ["mcp__"],
  aliases: {},
  specialTitles: { skillNames: [], scheduleNames: [], harnessNames: [] },
};

const PI: AgentProfile = {
  key: "pi",
  subagentToolNames: [],
  capabilities: {
    todos: false,
    skills: false,
    wakeup: false,
    subagents: false,
    legacyModeFallback: false,
    heartbeatKeepalives: false,
  },
  parentMetaNamespaces: [],
  mcpPrefixes: ["mcp__"],
  aliases: {},
  specialTitles: { skillNames: [], scheduleNames: [], harnessNames: [] },
};

const OMP: AgentProfile = {
  key: "omp",
  subagentToolNames: [],
  capabilities: {
    todos: false,
    skills: false,
    wakeup: false,
    subagents: false,
    legacyModeFallback: false,
    heartbeatKeepalives: false,
  },
  parentMetaNamespaces: [],
  mcpPrefixes: ["mcp__"],
  aliases: {},
  specialTitles: { skillNames: [], scheduleNames: [], harnessNames: [] },
};

const KIMI: AgentProfile = {
  key: "kimi",
  subagentToolNames: [],
  capabilities: {
    todos: false,
    skills: false,
    wakeup: false,
    subagents: false,
    legacyModeFallback: false,
    heartbeatKeepalives: false,
  },
  parentMetaNamespaces: [],
  mcpPrefixes: ["mcp__"],
  aliases: {},
  specialTitles: { skillNames: [], scheduleNames: [], harnessNames: [] },
};

const PRIME_AGENT: AgentProfile = {
  key: "prime-agent",
  subagentToolNames: [],
  capabilities: {
    todos: false,
    skills: false,
    wakeup: false,
    subagents: false,
    legacyModeFallback: false,
    heartbeatKeepalives: false,
  },
  parentMetaNamespaces: [],
  mcpPrefixes: ["mcp__"],
  aliases: {},
  specialTitles: { skillNames: [], scheduleNames: [], harnessNames: [] },
};

// aoe's own agent: three tools with real ToolKinds, no `_meta`, specials, modes, or heartbeats.
const AOE_AGENT: AgentProfile = {
  key: "aoe-agent",
  subagentToolNames: [],
  capabilities: {
    todos: false,
    skills: false,
    wakeup: false,
    subagents: false,
    legacyModeFallback: false,
    heartbeatKeepalives: false,
  },
  parentMetaNamespaces: [],
  mcpPrefixes: ["mcp__"],
  aliases: {},
  specialTitles: { skillNames: [], scheduleNames: [], harnessNames: [] },
};

/** Fallback for unknown agents: kind-only dispatch. */
export const DEFAULT_AGENT_PROFILE: AgentProfile = {
  key: "default",
  subagentToolNames: [],
  capabilities: {
    todos: false,
    skills: false,
    wakeup: false,
    subagents: false,
    legacyModeFallback: false,
    heartbeatKeepalives: false,
  },
  parentMetaNamespaces: [],
  mcpPrefixes: ["mcp__"],
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
  omp: OMP,
  kimi: KIMI,
  "prime-agent": PRIME_AGENT,
  "aoe-agent": AOE_AGENT,
};

export function resolveAgentProfile(toolKey: string | null | undefined): AgentProfile {
  if (!toolKey) return DEFAULT_AGENT_PROFILE;
  return PROFILES[toolKey] ?? DEFAULT_AGENT_PROFILE;
}

export function isSubagentToolName(rawName: string | null | undefined, profile: AgentProfile): boolean {
  if (!profile.capabilities.subagents || !rawName) return false;
  return profile.subagentToolNames.includes(rawName);
}

/** Mirror of `AgentProfile::is_clear_command` in src/acp/agent_profiles.rs. */
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
