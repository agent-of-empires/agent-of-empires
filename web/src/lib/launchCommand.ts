// Resolve a session's launch command with the backend's precedence: manual override, `agent_command_override`, `custom_agents`, then the agent's own command. The editable `prefix` is split from the fixed `suffix` (ACP registry args, or tmux extra args).

export interface ResolveLaunchCommandInput {
  tool: string;
  useStructuredView: boolean;
  /** Built-in binary, the tmux command and structured view fallback. */
  binary?: string;
  /** Preferred over `binary` for structured view sessions. */
  acpCommand?: string;
  acpArgs?: string[];
  /** Appended only for tmux sessions. */
  extraArgs?: string;
  /** Wins when set. */
  manualOverride?: string;
  agentCommandOverride?: Record<string, string>;
  customAgents?: Record<string, string>;
}

export interface ResolvedLaunchCommand {
  prefix: string;
  suffix: string;
  full: string;
}

export function resolveLaunchCommand(input: ResolveLaunchCommandInput): ResolvedLaunchCommand {
  const manual = input.manualOverride?.trim();
  const configOverride = input.agentCommandOverride?.[input.tool]?.trim();
  const custom = input.customAgents?.[input.tool]?.trim();

  let prefix: string;
  let suffix: string;

  if (input.useStructuredView) {
    prefix =
      manual || configOverride || custom || input.acpCommand?.trim() || input.binary?.trim() || input.tool.trim();
    // Registry args stay appended even with an override; custom agents have none.
    suffix = (input.acpArgs ?? []).join(" ").trim();
  } else {
    prefix = manual || configOverride || custom || input.binary?.trim() || input.tool.trim();
    suffix = input.extraArgs?.trim() ?? "";
  }

  // A stored override may already include the suffix; never show or spawn it twice.
  if (suffix && prefix.endsWith(` ${suffix}`)) {
    prefix = prefix.slice(0, prefix.length - suffix.length - 1).trimEnd();
  }

  const full = suffix ? `${prefix} ${suffix}` : prefix;
  return { prefix, suffix, full };
}
