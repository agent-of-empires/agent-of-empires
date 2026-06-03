import { useEffect, useState } from "react";
import { fetchSettings } from "../../lib/api";

export interface CommandMaps {
  agentCommandOverride: Record<string, string>;
  customAgents: Record<string, string>;
}

const EMPTY: CommandMaps = { agentCommandOverride: {}, customAgents: {} };

function asMap(v: unknown): Record<string, string> {
  return v && typeof v === "object"
    ? Object.fromEntries(
        Object.entries(v as Record<string, unknown>).filter(
          ([, val]) => typeof val === "string",
        ) as [string, string][],
      )
    : {};
}

/** Load the profile-resolved `agent_command_override` and `custom_agents`
 *  maps from settings. Used to preview the exact launch command in the
 *  new-session wizard, mirroring the backend `resolve_tool_command`
 *  precedence (#1766). Read-only; clears immediately on a profile change
 *  so a stale profile's override never flashes while the fetch is in
 *  flight. */
export function useCommandMaps(profile: string | undefined): CommandMaps {
  const [maps, setMaps] = useState<CommandMaps>(EMPTY);

  useEffect(() => {
    let cancelled = false;
    setMaps(EMPTY);
    void (async () => {
      try {
        const settings = await fetchSettings(profile || undefined);
        if (cancelled || !settings) return;
        const session = settings.session as Record<string, unknown> | undefined;
        setMaps({
          agentCommandOverride: asMap(session?.agent_command_override),
          customAgents: asMap(session?.custom_agents),
        });
      } catch {
        // A flaky settings fetch must not surface a wrong launch command;
        // fall back to empty maps (binary + registry args only).
        if (!cancelled) setMaps(EMPTY);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [profile]);

  return maps;
}
