/** Keep aligned with `ToolCards.renderToolCard`. */
export type CardKind = "execute" | "read" | "edit" | "delete" | "search" | "fetch" | "think";

/** Wire shape of `SessionResponse.agent_profile`; mirrors `AgentProfileWire` in src/acp/agent_profiles.rs. */
export interface AgentProfile {
  v: number;
  key: string;
  /** `_meta.<namespace>.parentToolUseId` lookup order; empty when linkage is unverified. */
  parent_meta_namespaces: string[];
  clear_aliases: string[];
  compact_aliases: string[];
  clear_requires_driven_reset: boolean;
  supports_exit_plan_mode: boolean;
  supports_wakeup_tools: boolean;
  emits_heartbeat_keepalives: boolean;
  yolo_mode_id: string | null;
  supports_subagents: boolean;
  todos: boolean;
  skills: boolean;
  /** May fall back to Claude's hardcoded mode taxonomy when the agent advertises none. */
  legacy_mode_fallback: boolean;
  /** `raw_name`s that launch an off-protocol subagent (opencode `task`), matched case-sensitively. */
  subagent_tool_names: string[];
  mcp_prefixes: string[];
  /** Tool names routed to a card when `tool.kind` doesn't indicate it. */
  card_aliases: Partial<Record<CardKind, string[]>>;
  special_titles: {
    skill_names: string[];
    schedule_names: string[];
    harness_names: string[];
    workflow_names: string[];
  };
  /** JS RegExp source matched against a tool's file path; null means the agent has no memory files. */
  memory_path_regex: string | null;
  mobile_monitor_only: boolean;
  fullscreen: boolean;
}

/** Kind-only dispatch, for a missing profile (an older daemon) or outside a session. */
export const DEFAULT_AGENT_PROFILE: AgentProfile = {
  v: 1,
  key: "default",
  parent_meta_namespaces: [],
  clear_aliases: [],
  compact_aliases: [],
  clear_requires_driven_reset: false,
  supports_exit_plan_mode: false,
  supports_wakeup_tools: false,
  emits_heartbeat_keepalives: false,
  yolo_mode_id: null,
  supports_subagents: false,
  todos: false,
  skills: false,
  legacy_mode_fallback: false,
  subagent_tool_names: [],
  mcp_prefixes: ["mcp__"],
  card_aliases: {},
  special_titles: { skill_names: [], schedule_names: [], harness_names: [], workflow_names: [] },
  memory_path_regex: null,
  mobile_monitor_only: false,
  fullscreen: false,
};

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
