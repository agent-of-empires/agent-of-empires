import { DEFAULT_AGENT_PROFILE, type AgentProfile } from "../agentProfile";

export function makeAgentProfile(over: Partial<AgentProfile> = {}): AgentProfile {
  return { ...DEFAULT_AGENT_PROFILE, ...over };
}

// The Rust test `serialized_profile_values_match_the_web_fixtures` in src/acp/agent_profiles.rs pins these values.
export const CLAUDE_PROFILE = makeAgentProfile({
  key: "claude",
  parent_meta_namespaces: ["claudeCode"],
  clear_aliases: ["/clear"],
  compact_aliases: ["/compact"],
  supports_wakeup_tools: true,
  emits_heartbeat_keepalives: true,
  supports_subagents: true,
  todos: true,
  skills: true,
  legacy_mode_fallback: true,
  special_titles: {
    skill_names: ["skill", "claude-skill"],
    schedule_names: ["ScheduleWakeup", "CronCreate", "CronList", "CronDelete"],
    harness_names: ["ToolSearch", "Monitor", "TaskStop", "SendMessage"],
    workflow_names: ["Workflow"],
  },
  memory_path_regex: "/\\.claude/projects/[^/]+/memory/.*\\.md$",
  mobile_monitor_only: true,
});

export const CODEX_PROFILE = makeAgentProfile({
  key: "codex",
  clear_aliases: ["/new"],
  supports_subagents: true,
  card_aliases: {
    execute: ["shell", "bash"],
    edit: ["apply_patch"],
    read: ["view_file", "read_file", "read"],
  },
});

export const OPENCODE_PROFILE = makeAgentProfile({
  key: "opencode",
  clear_aliases: ["/new"],
  todos: true,
  subagent_tool_names: ["task"],
  card_aliases: {
    execute: ["bash"],
    read: ["read"],
    edit: ["edit", "write"],
    search: ["grep", "glob"],
    fetch: ["webfetch"],
  },
});

const BY_KEY: Record<string, AgentProfile> = {
  claude: CLAUDE_PROFILE,
  codex: CODEX_PROFILE,
  opencode: OPENCODE_PROFILE,
};

/** Test stand-in for the daemon's per-agent profile; unknown keys get the default. */
export function profileFor(key: string | null | undefined): AgentProfile {
  return (key && BY_KEY[key]) || DEFAULT_AGENT_PROFILE;
}
