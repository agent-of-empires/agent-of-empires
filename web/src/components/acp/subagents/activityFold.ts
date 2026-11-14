// Folds a sub-agent's tool calls into display rows. Pure so the rules are testable.

import type { ActivityTool } from "./model";

export type ActivityFilter = "all" | "errors" | { kind: string };

export type ActivityNode =
  | { type: "row"; tool: ActivityTool }
  /** Hidden calls, never including an error. `run` is one kind, `span` a mixed middle. */
  | { type: "fold"; shape: "run" | "span"; tools: ActivityTool[] };

/** Up to this many calls show as rows, with nothing folded. */
export const SHORT_ACTIVITY = 8;
/** More raw calls than this show only a head and a tail, folding the middle. */
export const LONG_ACTIVITY = 20;
export const HEAD_ROWS = 3;
export const TAIL_ROWS = 4;
/** Adjacent same-kind calls fold from this many. */
export const MIN_RUN = 3;

export function applyFilter(tools: ActivityTool[], filter: ActivityFilter): ActivityTool[] {
  if (filter === "all") return tools;
  if (filter === "errors") return tools.filter((t) => t.status === "err");
  return tools.filter((t) => t.kind === filter.kind);
}

/** Errors stay rows; the calls between them fold when long enough. */
function foldSegments(tools: ActivityTool[], shape: "run" | "span", min: number): ActivityNode[] {
  const out: ActivityNode[] = [];
  let pending: ActivityTool[] = [];
  const flush = () => {
    if (pending.length >= min) out.push({ type: "fold", shape, tools: pending });
    else for (const tool of pending) out.push({ type: "row", tool });
    pending = [];
  };
  for (const tool of tools) {
    const breaks = tool.status === "err" || (shape === "run" && pending.length > 0 && pending[0]!.kind !== tool.kind);
    if (breaks) flush();
    if (tool.status === "err") out.push({ type: "row", tool });
    else pending.push(tool);
  }
  flush();
  return out;
}

const rows = (tools: ActivityTool[]) => tools.map((tool): ActivityNode => ({ type: "row", tool }));

/** A fold that would be the only node hides nothing worth a click, so the calls show as rows. */
export function foldActivity(tools: ActivityTool[]): ActivityNode[] {
  if (tools.length <= SHORT_ACTIVITY) return rows(tools);
  if (tools.length <= LONG_ACTIVITY) {
    const nodes = foldSegments(tools, "run", MIN_RUN);
    return nodes.length === 1 ? rows(tools) : nodes;
  }
  const head = tools.slice(0, HEAD_ROWS);
  const tail = tools.slice(-TAIL_ROWS);
  const middle = tools.slice(HEAD_ROWS, tools.length - TAIL_ROWS);
  return [...rows(head), ...foldSegments(middle, "span", 2), ...rows(tail)];
}

const KIND_LABELS: Record<string, [string, string]> = {
  execute: ["command", "commands"],
  read: ["read", "reads"],
  edit: ["edit", "edits"],
  delete: ["delete", "deletes"],
  search: ["search", "searches"],
  fetch: ["fetch", "fetches"],
  think: ["think", "thinks"],
  agent: ["agent", "agents"],
  mcp: ["MCP call", "MCP calls"],
  other: ["call", "calls"],
};

export function kindLabel(kind: string, count: number): string {
  const [one, many] = KIND_LABELS[kind] ?? [kind, `${kind}s`];
  return count === 1 ? one : many;
}

/** "6 reads" for a run; "9 calls: 5 reads, 4 commands" for a mixed span. A fold holds no error. */
export function foldLabel(node: Extract<ActivityNode, { type: "fold" }>): string {
  const counts = new Map<string, number>();
  for (const t of node.tools) counts.set(t.kind, (counts.get(t.kind) ?? 0) + 1);
  if (node.shape === "run" || counts.size === 1) {
    const [kind, n] = [...counts][0]!;
    return `${n} ${kindLabel(kind, n)}`;
  }
  const parts = [...counts].sort((a, b) => b[1] - a[1]).map(([k, n]) => `${n} ${kindLabel(k, n)}`);
  return `${node.tools.length} calls: ${parts.join(", ")}`;
}

/** While following a running agent: only the newest rows, the rest one fold. */
export function followTail(tools: ActivityTool[], keep = 8): ActivityNode[] {
  if (tools.length <= keep) return rows(tools);
  return [...foldSegments(tools.slice(0, tools.length - keep), "span", 2), ...rows(tools.slice(-keep))];
}
