// Tools known to have a published ACP server. Anything not in this
// set falls back to tmux automatically; when the structured view master
// switch is on, the wizard creates structured view sessions only for tools
// listed here.
//
// SOURCE OF TRUTH: src/acp/agent_registry.rs. If you add a new
// ACP adapter to that registry, also add it here; otherwise the web
// wizard will silently fall back to tmux for it. (Long-term we should
// expose this list via /api/about and drop the JS-side copy.)
export const ACP_CAPABLE_TOOLS: ReadonlySet<string> = new Set([
  "claude",
  "opencode",
  "gemini",
  "codex",
  "vibe",
  "pi",
  "omp",
]);

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

/** Whether the wizard may create a structured view session for `tool`:
 *  acp-capable AND permitted by the operator's `[acp] allowed_agents`
 *  policy (#3241). Capability and permission are separate axes on
 *  purpose, see the `acp_allowed` field docs; this is the conjunction the
 *  wizard wants, so it does not offer a structured session the server
 *  would then refuse with a 403.
 *
 *  Only an explicit `false` denies. An absent `acp_allowed` means an older
 *  server or a test fixture that predates the field, which is treated as
 *  permitted for the same reason `isAcpCapable` falls back on an undefined
 *  flag: the wizard must not lock itself out against a server that never
 *  reports the field. */
export function isAcpEligible(
  tool: string,
  agent: { acp_capable?: boolean; acp_allowed?: boolean } | undefined,
): boolean {
  if (agent?.acp_allowed === false) return false;
  return isAcpCapable(tool, agent?.acp_capable);
}
