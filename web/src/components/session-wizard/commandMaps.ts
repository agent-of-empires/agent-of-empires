export interface CommandMaps {
  agentCommandOverride: Record<string, string>;
  customAgents: Record<string, string>;
}

export const EMPTY_COMMAND_MAPS: CommandMaps = {
  agentCommandOverride: {},
  customAgents: {},
};

function asMap(v: unknown): Record<string, string> {
  return v && typeof v === "object"
    ? Object.fromEntries(
        Object.entries(v as Record<string, unknown>).filter(([, val]) => typeof val === "string") as [string, string][],
      )
    : {};
}

/** Override and custom-agent maps for the launch command preview; empty for a malformed payload. */
export function commandMapsFromSettings(settings: unknown): CommandMaps {
  if (!settings || typeof settings !== "object") return EMPTY_COMMAND_MAPS;
  const session = (settings as Record<string, unknown>).session as Record<string, unknown> | undefined;
  return {
    agentCommandOverride: asMap(session?.agent_command_override),
    customAgents: asMap(session?.custom_agents),
  };
}
