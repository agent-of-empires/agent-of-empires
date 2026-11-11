// Fallback list of tools with an ACP server; mirror src/acp/agent_registry.rs.
export const ACP_CAPABLE_TOOLS: ReadonlySet<string> = new Set([
  "claude",
  "opencode",
  "gemini",
  "codex",
  "vibe",
  "pi",
  "omp",
  "kimi",
  "prime-agent",
]);

/** Prefer the server's `acp_capable`; the static set covers the window before agents load. */
export function isAcpCapable(tool: string, flag: boolean | undefined): boolean {
  if (typeof flag === "boolean") return flag;
  return ACP_CAPABLE_TOOLS.has(tool);
}

/** Capable and allowed by `[acp] allowed_agents`. Only an explicit `false` denies, so older servers stay usable.
 *  `acp_create_ready: true` counts as capable, since a repository alias can make an agent ACP-capable. */
export function isAcpEligible(
  tool: string,
  agent: { acp_capable?: boolean; acp_allowed?: boolean; acp_create_ready?: boolean } | undefined,
): boolean {
  if (agent?.acp_allowed === false) return false;
  if (agent?.acp_create_ready === true) return true;
  return isAcpCapable(tool, agent?.acp_capable);
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
  tool: string,
  agent: { acp_capable?: boolean; acp_allowed?: boolean; acp_create_ready?: boolean } | undefined,
  launch: { sandboxEnabled: boolean; commandOverride: string },
): boolean {
  return isAcpEligible(tool, agent) && !acpAdapterMissing(agent, launch);
}
