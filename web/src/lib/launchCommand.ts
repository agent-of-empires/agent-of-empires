// Resolve the launch command a session will run for a given tool,
// mirroring the backend `SessionConfig::resolve_tool_command`
// precedence (src/session/config.rs): a manual per-session override
// wins, then `session.agent_command_override`, then `custom_agents`,
// otherwise the agent's own binary (or the tool name as a last resort).
//
// Used by the new-session wizard to preview the exact command before a
// session starts, so a configured override (e.g. opencode →
// opencode-plannotator) is visible up front rather than a surprise once
// the agent launches. See #1766.

export interface ResolveLaunchCommandInput {
  /** Selected tool / agent name, e.g. "opencode". */
  tool: string;
  /** The agent's built-in binary, shown when no override applies. */
  binary?: string;
  /** Manual per-session override typed in the wizard. Wins when set. */
  manualOverride?: string;
  /** `session.agent_command_override` map from settings. */
  agentCommandOverride?: Record<string, string>;
  /** `session.custom_agents` map from settings. */
  customAgents?: Record<string, string>;
}

export function resolveLaunchCommand(input: ResolveLaunchCommandInput): string {
  const manual = input.manualOverride?.trim();
  if (manual) return manual;

  const override = input.agentCommandOverride?.[input.tool]?.trim();
  if (override) return override;

  const custom = input.customAgents?.[input.tool]?.trim();
  if (custom) return custom;

  return input.binary?.trim() || input.tool.trim();
}
