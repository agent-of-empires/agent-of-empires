// OMP is the only bundled ACP-capable tool in this fork.
export const ACP_CAPABLE_TOOLS: ReadonlySet<string> = new Set(["omp"]);

/** Authoritative acp-capability check. The server now reports
 *  `acp_capable` per agent (built-ins and custom agents with an
 *  `agent_acp_cmd`), so prefer that. The hardcoded set above is only
 *  a fallback for the brief window before the agent/session list loads,
 *  or older servers that don't yet send the field; it never reflects
 *  custom agents. */
export function isAcpCapable(tool: string, flag: boolean | undefined): boolean {
  if (typeof flag === "boolean") return flag;
  return ACP_CAPABLE_TOOLS.has(tool);
}
