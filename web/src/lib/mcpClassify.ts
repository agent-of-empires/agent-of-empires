// Recognise `mcp__<server>__<verb>` tool calls, which adapters send as `kind: "other"`, using the profile's `mcpPrefixes`.

import type { ToolCall } from "./acpTypes";
import { parseJsonObject, pickStr } from "./acpArgs";
import type { AgentProfile } from "./agentProfiles";

const DEFAULT_MCP_PREFIXES = ["mcp__"];

export interface McpHit {
  isMcp: true;
  server: string;
  verb: string;
}

export interface NotMcp {
  isMcp: false;
}

/** Prefer the wire name, else the forwarded `_aoe_title`. */
function nameOf(tool: ToolCall): string {
  if (tool.name) return tool.name;
  const t = pickStr(parseJsonObject(tool.args_preview), "_aoe_title");
  return t ?? "";
}

/** Server names may contain single underscores; separators are always `__`. */
function parseMcpName(name: string, prefix: string): { server: string; verb: string } | null {
  if (!name.startsWith(prefix)) return null;
  const parts = name.split("__");
  if (parts.length < 3) return null;
  const prefixHead = prefix.replace(/__$/, "");
  if (parts[0] !== prefixHead) return null;
  const server = parts[1];
  const verb = parts.slice(2).join("__");
  if (!server || !verb) return null;
  return { server, verb };
}

export function classifyMcp(tool: ToolCall, profile?: AgentProfile | null): McpHit | NotMcp {
  const name = nameOf(tool);
  const prefixes = profile?.mcpPrefixes ?? DEFAULT_MCP_PREFIXES;
  for (const prefix of prefixes) {
    const hit = parseMcpName(name, prefix);
    if (hit) {
      return { isMcp: true, server: hit.server, verb: hit.verb };
    }
  }
  return { isMcp: false };
}

/** Keeps mixed-case chunks, title-cases lowercase ones. */
export function humanizeServer(server: string): string {
  return server
    .split(/[-_]/)
    .filter(Boolean)
    .map((c) => (/[A-Z]/.test(c) ? c : c.charAt(0).toUpperCase() + c.slice(1)))
    .join(" ");
}

/** `get_sentry_resource` becomes `Get sentry resource`. */
export function humanizeVerb(verb: string): string {
  const spaced = verb.replace(/_/g, " ").trim();
  if (!spaced) return "";
  return spaced.charAt(0).toUpperCase() + spaced.slice(1);
}
