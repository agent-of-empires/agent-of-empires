// One sub-agent as the shared view reads it, whether it ran inline (a Task
// tool call with children) or in the background (a tailed transcript).

import { parseJsonObject, pickStr } from "../../../lib/acpArgs";
import type {
  ActivityRow,
  BackgroundAgent,
  BackgroundAgentTool,
  SubagentRecord as RosterEntry,
  ToolCall,
} from "../../../lib/acpTypes";
import { statusFor, type ToolCardProps } from "../ToolCardChrome";

export type SubagentStatus = "running" | "waiting" | "done" | "failed" | "stalled" | "detached" | "unknown";
export type ToolStatus = "running" | "ok" | "err" | "stopped";

export interface ActivityTool {
  id: string;
  /** execute, read, edit, delete, search, fetch, think, agent or other. */
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
  /** The roster's own state, shown beside the status. */
  badge?: string;
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
  const target = args ? pickStr(args, ...TARGET_KEYS) : null;
  const known = tool.kind && tool.kind !== "other" ? tool.kind : kindForName(tool.name);
  return {
    id: tool.id,
    kind: known,
    name: tool.name,
    target: target ? firstLine(target) : null,
    status: !result && taskDone ? "stopped" : statusFor(result),
    startedAt: tool.started_at,
    endedAt: result?.at,
    call: { tool, result },
  };
}

function bgStatus(s: BackgroundAgent["status"]): SubagentStatus {
  return s === "completed" ? "done" : s === "error" ? "failed" : s === "requires_action" ? "waiting" : s;
}

function rosterBadge(r: RosterEntry): string {
  switch (r.state) {
    case "running":
      return "running";
    case "requires_action":
      return "waiting on you";
    case "idle":
      return r.stop_reason ? `idle \u00b7 ${r.stop_reason}` : "idle";
    default:
      return "unknown";
  }
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

/** A sync Task or delegation: its own call and result plus the calls it made. The roster,
 *  when it knows the child, outranks the launch call: a background child outlives the
 *  turn that stopped that call, and an idle one ends the card even with no reply. */
export function fromInline(
  tool: ToolCall,
  toolResult: ActivityRow | undefined,
  children: ToolCardProps[],
  roster?: RosterEntry,
): SubagentRecord {
  const args = parseJsonObject(tool.args_preview);
  const result = roster && roster.state !== "unknown" && toolResult?.kind === "tool_stopped" ? undefined : toolResult;
  const idle = roster?.state === "idle";
  const pending = result === undefined;
  const failed = result ? result.kind === "tool_error" : roster?.stop_reason === "error";
  const running = pending && !idle;
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
    background: false,
    status: running ? "running" : failed ? "failed" : "done",
    badge: roster && rosterBadge(roster),
    startedAt: tool.started_at,
    // The roster has no end time, so a child settled without a reply shows no duration.
    endedAt: running || (idle && !result) ? undefined : (result?.at ?? lastEnd),
    prompt: tool.prompt ?? taskPrompt(tool.args_preview),
    tools,
    toolCount: tools.length,
    lastTool: tools.at(-1)?.name ?? null,
    report: failed || !result ? "" : extractTaskResult(result.text ?? ""),
    latestNote: null,
    warning: null,
    error: failed ? (result?.text ?? "") : null,
  };
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
