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
        Object.entries(v as Record<string, unknown>).filter(
          ([, val]) => typeof val === "string",
        ) as [string, string][],
      )
    : {};
}

/** Extract the `agent_command_override` and `custom_agents` maps from a
 *  settings payload, used to preview the exact launch command in the
 *  new-session wizard (mirrors the backend `resolve_tool_command`
 *  precedence, #1766). Pure: the wizard fetches settings once on open
 *  (and again on a profile switch) and derives the maps from that, so
 *  the preview never issues a second settings request. Returns empty
 *  maps for a missing or malformed payload, so a flaky fetch falls back
 *  to binary + registry args rather than a wrong command. */
export function commandMapsFromSettings(settings: unknown): CommandMaps {
  if (!settings || typeof settings !== "object") return EMPTY_COMMAND_MAPS;
  const session = (settings as Record<string, unknown>).session as
    | Record<string, unknown>
    | undefined;
  return {
    agentCommandOverride: asMap(session?.agent_command_override),
    customAgents: asMap(session?.custom_agents),
  };
}
