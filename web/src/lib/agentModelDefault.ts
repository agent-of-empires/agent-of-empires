import type { ServerAbout } from "./api";

export {
  fetchSessionModelDefault as loadSessionModelDefault,
  putAgentModelDefault as saveAgentModelDefault,
  type AgentModelDefault,
} from "./api";

/** The Settings row for the profile's Agent defaults, or null on read-only, CityHall, or before /api/about answers. */
export function agentDefaultsHref(profile: string, about: ServerAbout | null | undefined): string | null {
  if (!about || about.read_only || about.cityhall_mode) return null;
  return `/settings/structured-view?profile=${encodeURIComponent(profile)}&field=acp.acp_defaults`;
}
