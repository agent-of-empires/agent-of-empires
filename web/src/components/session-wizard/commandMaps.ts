import type { SettingsResponse } from "../../lib/api";

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
export function commandMapsFromSettings(settings: SettingsResponse | null | undefined): CommandMaps {
  if (!settings || typeof settings !== "object") return EMPTY_COMMAND_MAPS;
  return {
    agentCommandOverride: asMap(settings.session?.agent_command_override),
    customAgents: asMap(settings.session?.custom_agents),
  };
}
