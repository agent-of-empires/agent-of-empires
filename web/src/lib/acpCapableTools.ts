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

/** Capable and allowed by `[acp] allowed_agents`. Only an explicit `false` denies, so older servers stay usable. */
export function isAcpEligible(
  tool: string,
  agent: { acp_capable?: boolean; acp_allowed?: boolean } | undefined,
): boolean {
  if (agent?.acp_allowed === false) return false;
  return isAcpCapable(tool, agent?.acp_capable);
}
