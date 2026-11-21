import type { ServerAbout } from "./api";

export {
  fetchAgentModelDefault as loadAgentModelDefault,
  putAgentModelDefault as saveAgentModelDefault,
  type AgentModelDefault,
} from "./api";

/** Where a Session's model picker can save its Agent's default; null where Settings can't be written. */
export interface AgentDefaultsTarget {
  profile: string;
  /** Opens Settings › Structured view at the Agent defaults row. */
  open: () => void;
}

/** The Settings row for the profile's Agent defaults, or null on read-only, CityHall, or before /api/about answers. */
export function agentDefaultsHref(profile: string, about: ServerAbout | null | undefined): string | null {
  if (!about || about.read_only || about.cityhall_mode) return null;
  return `/settings/structured-view?profile=${encodeURIComponent(profile)}&field=acp.acp_defaults`;
}
