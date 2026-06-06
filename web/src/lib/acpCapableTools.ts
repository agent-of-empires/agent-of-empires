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
