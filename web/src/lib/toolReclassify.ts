// Render pure search shellouts (grep, rg, find) from ACP "execute" calls in the search card; the Rust `kind` stays faithful.

import type { ToolCall } from "./acpTypes";
import { parseJsonObject, pickStr } from "./acpArgs";

const SEARCH_BIN = /^\s*(?:ripgrep|rg|grep|egrep|fgrep|ack|ag|find|fd)\b/;

// Pipes, chaining, redirects, or destructive find flags make a call more than a read. `<=` is allowed inside patterns.
const MUTATING = /[|;&]|>{1,2}|<(?!=)|-delete\b|-exec\b|--exec\b/;

export interface Reclassified {
  kind: string;
  /** "bash" when reclassified, so the card can show "search · bash". */
  provenance: "bash" | null;
}

export function reclassifyBash(tool: ToolCall): Reclassified {
  if (tool.kind !== "execute") {
    return { kind: tool.kind, provenance: null };
  }
  const command = pickStr(parseJsonObject(tool.args_preview), "command")?.trim();
  if (!command) {
    return { kind: tool.kind, provenance: null };
  }
  if (SEARCH_BIN.test(command) && !MUTATING.test(command)) {
    return { kind: "search", provenance: "bash" };
  }
  return { kind: tool.kind, provenance: null };
}
