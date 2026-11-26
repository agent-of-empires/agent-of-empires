/** The server's `acp_capable` flag; false until agents load. */
export function isAcpCapable(flag: boolean | undefined): boolean {
  return flag === true;
}

/** Capable and allowed by `[acp] allowed_agents`. Only an explicit `false` denies, so older servers stay usable.
 *  `acp_create_ready: true` counts as capable, since a repository alias can make an agent ACP-capable. */
export function isAcpEligible(
  agent: { acp_capable?: boolean; acp_allowed?: boolean; acp_create_ready?: boolean } | undefined,
): boolean {
  if (agent?.acp_allowed === false) return false;
  if (agent?.acp_create_ready === true) return true;
  return isAcpCapable(agent?.acp_capable);
}

/** The Daemon would refuse a structured create for a missing adapter. Only an explicit
 *  `acp_create_ready: false` counts, so older servers stay usable. A Container bundles the
 *  adapter and a command override is checked at create, so neither is gated here. */
export function acpAdapterMissing(
  agent: { acp_create_ready?: boolean } | undefined,
  launch: { sandboxEnabled: boolean; commandOverride: string },
): boolean {
  return agent?.acp_create_ready === false && !launch.sandboxEnabled && !launch.commandOverride.trim();
}

/** Structured view can be chosen: eligible, and its adapter is not known to be missing. */
export function structuredAvailable(
  agent: { acp_capable?: boolean; acp_allowed?: boolean; acp_create_ready?: boolean } | undefined,
  launch: { sandboxEnabled: boolean; commandOverride: string },
): boolean {
  return isAcpEligible(agent) && !acpAdapterMissing(agent, launch);
}
