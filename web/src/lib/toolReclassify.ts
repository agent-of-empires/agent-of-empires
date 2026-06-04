// Tool-card presentation heuristic: reclassify ACP "execute" (Bash) tool
// calls that are actually pure search shellouts (grep, rg, find, fd, …)
// so the structured view renders them in the search card instead of the heavier
// bash card. The Rust `kind` stays faithful to ACP; this is purely a
// frontend rendering concern.

import type { ToolCall } from "./acpTypes";
import { parseJsonObject, pickStr } from "./acpArgs";

// Command starts with a known read-only search binary. `ripgrep` covers
// the long form that some users still type out of habit.
const SEARCH_BIN = /^\s*(?:ripgrep|rg|grep|egrep|fgrep|ack|ag|find|fd)\b/;

// Anything that turns the call into something other than a pure read:
// pipes, command chaining, file redirects, or destructive find flags.
// `<=` is excluded so `grep` doesn't trip on comparison-like fragments
// inside a pattern argument that the user genuinely typed.
const MUTATING = /[|;&]|>{1,2}|<(?!=)|-delete\b|-exec\b|--exec\b/;

export interface Reclassified {
  /** Effective tool kind to dispatch to (may differ from tool.kind). */
  kind: string;
  /** Origin of the call when reclassified; used to surface "search · bash"
   *  on the card so the swap stays transparent. Null when not reclassified. */
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
