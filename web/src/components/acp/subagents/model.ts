// One sub-agent as the shared view reads it, whether it ran inline (a Task
// tool call with children) or in the background (a tailed transcript).

import { parseJsonObject, pickStr } from "../../../lib/acpArgs";
import { classifyMcp, humanizeServer, humanizeVerb } from "../../../lib/mcpClassify";
import type {
  ActivityRow,
  BackgroundAgent,
  BackgroundAgentTool,
  SubagentRecord as RosterEntry,
  ToolCall,
} from "../../../lib/acpTypes";
import { statusFor, type ToolCardProps } from "../ToolCardChrome";

export type SubagentStatus = "running" | "waiting" | "done" | "stopped" | "failed" | "stalled" | "detached" | "unknown";
export type ToolStatus = "running" | "ok" | "err" | "stopped";

export interface ActivityTool {
  id: string;
  /** execute, read, edit, delete, search, fetch, think, agent, mcp or other. */
  kind: string;
  name: string;
  target: string | null;
  /** The whole title, which a row shows only the first line of. */
  detail?: string;
  status: ToolStatus;
  startedAt?: string;
  endedAt?: string;
  /** Inline calls open as their regular tool card. */
  call?: ToolCardProps;
}

export interface SubagentRecord {
  /** The launching Task call, also the thread anchor. */
  id: string;
  description: string;
  background: boolean;
  status: SubagentStatus;
  startedAt?: string;
  endedAt?: string;
  model?: string;
  prompt: string;
  tools: ActivityTool[];
  /** Calls made in total; `tools` is capped, so this can be larger. */
  toolCount: number;
  /** Name of the newest call, which `tools` may not reach. */
  lastTool: string | null;
  report: string;
  /** Latest assistant note while running; not the final report. */
  latestNote: string | null;
  warning: string | null;
  error: string | null;
}

/** Unwrap opencode's `<task><task_result>…</task_result></task>` report; anything else is returned verbatim. */
export function extractTaskResult(text: string): string {
  const match = text.trim().match(/^<task\b[^>]*>\s*<task_result\b[^>]*>([\s\S]*?)<\/task_result>\s*<\/task>$/);
  return match ? match[1]!.trim() : text;
}

const TARGET_KEYS = ["command", "file_path", "path", "pattern", "url", "query", "description"];

const NAME_KINDS: Record<string, string> = {
  Bash: "execute",
  Read: "read",
  Edit: "edit",
  MultiEdit: "edit",
  Write: "edit",
  NotebookEdit: "edit",
  Grep: "search",
  Glob: "search",
  WebSearch: "search",
  WebFetch: "fetch",
  Task: "agent",
  Agent: "agent",
};

export function kindForName(name: string): string {
  return NAME_KINDS[name] ?? "other";
}

const TRUNCATED = "\u2026[truncated]";

/** A Task's prompt from its args preview; one cut at the ingest cap is no valid JSON, so its prompt is recovered from the prefix. */
export function taskPrompt(argsPreview: string): string {
  const args = parseJsonObject(argsPreview);
  if (args) return pickStr(args, "prompt") ?? "";
  const body = /"prompt"\s*:\s*"((?:[^"\\]|\\.)*)/.exec(argsPreview)?.[1];
  if (!body) return "";
  const text = (body.endsWith(TRUNCATED) ? body.slice(0, -TRUNCATED.length) : body).replace(
    /\\u[0-9a-fA-F]{0,3}$|\\$/,
    "",
  );
  try {
    return `${JSON.parse(`"${text}"`) as string}\n\n${TRUNCATED}`;
  } catch {
    return "";
  }
}

function firstLine(text: string): string {
  return text.split("\n", 1)[0]!.trim();
}

function inlineTool({ tool, result }: ToolCardProps, taskDone: boolean): ActivityTool {
  const args = parseJsonObject(tool.args_preview);
  // The wire name survives the retitle that makes `name` the command or path.
  const raw = (args && pickStr(args, "_aoe_raw_tool_name")) || tool.raw_name || tool.name;
  const mcp = classifyMcp({ ...tool, name: raw });
  const name = mcp.isMcp ? `${humanizeServer(mcp.server)} ${humanizeVerb(mcp.verb)}` : raw;
  const target = args && pickStr(args, ...TARGET_KEYS);
  const line = target ? firstLine(target) : null;
  const known = tool.kind && tool.kind !== "other" ? tool.kind : mcp.isMcp ? "mcp" : kindForName(raw);
  return {
    id: tool.id,
    kind: known,
    name,
    target: line === name ? null : line,
    status: !result && taskDone ? "stopped" : statusFor(result),
    startedAt: tool.started_at,
    endedAt: result?.at,
    call: { tool, result },
  };
}

function bgStatus(s: BackgroundAgent["status"]): SubagentStatus {
  return s === "completed" ? "done" : s === "error" ? "failed" : s === "requires_action" ? "waiting" : s;
}

/** A settled child's status from how its launch call ended, else from its idle roster entry. */
function settledStatus(result: ActivityRow | undefined, roster?: RosterEntry): SubagentStatus {
  const kind = result?.kind;
  const reason =
    kind === "tool_error" ? "error" : kind === "tool_stopped" ? "cancelled" : result ? null : roster?.stop_reason;
  return reason === "error" ? "failed" : reason === "cancelled" ? "stopped" : "done";
}

function backgroundTool(t: BackgroundAgentTool, i: number, ended: boolean): ActivityTool {
  return {
    id: t.id ?? `#${i}`,
    kind: kindForName(t.name),
    name: t.name,
    target: t.title ? firstLine(t.title) : null,
    detail: t.title || undefined,
    status: t.ok === undefined || t.ok === null ? (ended ? "stopped" : "running") : t.ok ? "ok" : "err",
    startedAt: t.started_at ?? undefined,
    endedAt: t.ended_at ?? undefined,
  };
}

/** A sync Task or delegation: its own call and result plus the calls it made. A background
 *  child outlives the turn that stopped its launch call, so its roster entry decides; an
 *  idle one ends the card even with no reply. */
export function fromInline(
  tool: ToolCall,
  toolResult: ActivityRow | undefined,
  children: ToolCardProps[],
  roster?: RosterEntry,
): SubagentRecord {
  const args = parseJsonObject(tool.args_preview);
  const following = roster?.background && roster.state !== "unknown";
  const result = following && toolResult?.kind === "tool_stopped" ? undefined : toolResult;
  const running = !result && roster?.state !== "idle";
  const status: SubagentStatus = running
    ? roster?.state === "requires_action"
      ? "waiting"
      : "running"
    : settledStatus(result, roster);
  const tools = children.map((c) => inlineTool(c, !running));
  const lastEnd = tools
    .map((t) => t.endedAt)
    .filter((t): t is string => !!t)
    .sort()
    .at(-1);
  return {
    id: tool.id,
    description:
      roster?.title || ((args && pickStr(args, "description", "_aoe_title")) ?? tool.name ?? "Subagent task"),
    background: roster?.background ?? false,
    status,
    startedAt: tool.started_at,
    // The roster has no end time, so a child settled without a reply shows no duration.
    endedAt: result ? (result.at ?? lastEnd) : undefined,
    prompt: tool.prompt ?? taskPrompt(tool.args_preview),
    tools,
    toolCount: tools.length,
    lastTool: tools.at(-1)?.name ?? null,
    report: status === "done" && result ? extractTaskResult(result.text ?? "") : "",
    latestNote: null,
    warning: null,
    error: status === "failed" ? result?.text || roster?.error || "" : null,
  };
}

/** A launch card's call, result and child calls, read from a session's activity rows. */
export function launchFromRows(
  rows: readonly ActivityRow[],
  id: string,
): { tool: ToolCall; result?: ActivityRow; children: ToolCardProps[] } | undefined {
  let tool: ToolCall | undefined;
  let result: ActivityRow | undefined;
  const children = new Map<string, ToolCardProps>();
  for (const row of rows) {
    if (row.kind === "tool_start" && row.tool) {
      if (row.tool.id === id) tool = row.tool;
      else if (row.tool.parent_tool_call_id === id) children.set(row.tool.id, { tool: row.tool });
    } else if (row.kind === "tool_complete" || row.kind === "tool_error" || row.kind === "tool_stopped") {
      const done = row.toolCallId ?? row.id.replace(/^(done|stopped)-/, "");
      const child = children.get(done);
      if (done === id) result = row;
      else if (child) child.result = row;
    }
  }
  return tool && { tool, result, children: [...children.values()] };
}

/** A tailed background agent. Its tools carry no inline card. */
export function fromBackground(agent: BackgroundAgent): SubagentRecord {
  const status = bgStatus(agent.status);
  return {
    id: agent.toolCallId,
    description: agent.description || "Sub-agent",
    background: true,
    status,
    startedAt: agent.startedAt,
    endedAt: agent.endedAt ?? undefined,
    model: agent.model || undefined,
    prompt: agent.prompt,
    tools: agent.tools.map((t, i) => backgroundTool(t, i, agent.endedAt !== null)),
    toolCount: Math.max(agent.toolCount, agent.tools.length),
    lastTool: agent.lastTool,
    report: agent.result ?? "",
    latestNote: status === "running" ? agent.lastText : null,
    warning: status === "failed" ? null : agent.warning,
    error: status === "failed" ? (agent.warning ?? "background agent error") : null,
  };
}
