// Resolve the launch command a session will run for a given tool,
// mirroring the backend `SessionConfig::resolve_tool_command` and the
// cockpit supervisor's `apply_agent_command_override` precedence: a
// manual per-session override wins, then `session.agent_command_override`,
// then `session.custom_agents`, otherwise the agent's own command.
//
// The result is split into an editable `prefix` (the command, which a
// per-session override replaces) and a read-only `suffix` (the args that
// are always appended). For a cockpit session the suffix is the ACP
// registry args (e.g. `acp` for opencode); for a tmux session it is the
// session's extra args. Keeping the suffix separate lets the wizard show
// the full resolved command while editing only the override portion, so
// editing never duplicates the registry args. See #1766 and #1911.

export interface ResolveLaunchCommandInput {
  /** Selected tool / agent name, e.g. "opencode". */
  tool: string;
  /** Whether this session will run in cockpit (vs the tmux terminal).
   *  Drives which suffix applies and which base command is used. */
  useCockpit: boolean;
  /** The agent's built-in binary, used as the tmux base command and as
   *  the cockpit fallback when no registry command is known. */
  binary?: string;
  /** The cockpit ACP command for a built-in agent (e.g.
   *  `claude-agent-acp`), from `AgentInfo.cockpit_command`. Differs from
   *  `binary` and is preferred for cockpit sessions. */
  cockpitCommand?: string;
  /** Registry args appended in cockpit (e.g. `["acp"]`), from
   *  `AgentInfo.cockpit_args`. */
  cockpitArgs?: string[];
  /** The session's extra args, appended only for tmux sessions. */
  extraArgs?: string;
  /** Manual per-session override typed in the wizard. Wins when set. */
  manualOverride?: string;
  /** `session.agent_command_override` map from settings. */
  agentCommandOverride?: Record<string, string>;
  /** `session.custom_agents` map from settings. */
  customAgents?: Record<string, string>;
}

export interface ResolvedLaunchCommand {
  /** The command portion, editable as a per-session command override. */
  prefix: string;
  /** The args that are always appended, never editable here. */
  suffix: string;
  /** `prefix` and `suffix` joined for display. */
  full: string;
}

export function resolveLaunchCommand(
  input: ResolveLaunchCommandInput,
): ResolvedLaunchCommand {
  const manual = input.manualOverride?.trim();
  const configOverride = input.agentCommandOverride?.[input.tool]?.trim();
  const custom = input.customAgents?.[input.tool]?.trim();

  let prefix: string;
  let suffix: string;

  if (input.useCockpit) {
    prefix =
      manual ||
      configOverride ||
      custom ||
      input.cockpitCommand?.trim() ||
      input.binary?.trim() ||
      input.tool.trim();
    // Registry args are always appended for built-in cockpit agents and
    // are retained even when a command override is set. Custom agents
    // carry no registry args, so the suffix is empty for them.
    suffix = (input.cockpitArgs ?? []).join(" ").trim();
  } else {
    prefix =
      manual ||
      configOverride ||
      custom ||
      input.binary?.trim() ||
      input.tool.trim();
    suffix = input.extraArgs?.trim() ?? "";
  }

  // Guard against a stored override that already includes the fixed
  // suffix (e.g. a pasted "opencode acp" with cockpitArgs ["acp"]) so the
  // suffix is never shown, or spawned, twice.
  if (suffix && prefix.endsWith(` ${suffix}`)) {
    prefix = prefix.slice(0, prefix.length - suffix.length - 1).trimEnd();
  }

  const full = suffix ? `${prefix} ${suffix}` : prefix;
  return { prefix, suffix, full };
}
