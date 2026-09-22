// Converts the flat ActivityRow log into assistant-ui's message tree. Each user
// row opens a user message; agent rows collapse into one assistant message whose
// tool-call parts are completed in place and folded into subagent and run groups.
// ThreadMessages.tsx rebuilds ToolCalls from the `_aoe_*` keys smuggled into argsText.

import type { ThreadMessageLike } from "@assistant-ui/react";

import { hasTodoArrayArgsText, parseJsonObject } from "../../lib/acpArgs";
import { lastClearIndex } from "../../lib/acpHistoryWindow";
import type { ActivityRow, ToolCall } from "../../lib/acpTypes";
import { type AgentProfile, DEFAULT_AGENT_PROFILE, isSubagentToolName } from "../../lib/agentProfiles";

/** Synthetic part for a subagent Task with its child tool calls. */
export const SUBAGENT_TASK_NAME = "_aoe_subagent_task";
/** Synthetic part for a folded run of tool calls. */
export const TOOL_GROUP_NAME = "_aoe_tool_group";
/** Synthetic part for a folded run of todo snapshots. */
export const TODO_GROUP_NAME = "_aoe_todo_group";

/** Two in a row stay inline; three or more fold. */
const TOOL_GROUP_MIN_RUN = 3;

/** React key for the fold point: the last `/clear` id, "none" before one, "all" when unfolded. */
export function clearFoldGeneration(rows: readonly ActivityRow[], showClearedTurns: boolean): string {
  if (showClearedTurns) return "all";
  const i = lastClearIndex(rows);
  return i < 0 ? "none" : rows[i]!.id;
}

function parseDate(iso: string): Date | undefined {
  const d = new Date(iso);
  return Number.isFinite(d.getTime()) ? d : undefined;
}

/** Rows rendered as an assistant blockquote callout, by kind. */
const CALLOUTS: Partial<Record<ActivityRow["kind"], (text: string) => string>> = {
  session_cleared: (text) => `> ⚠️ **Conversation cleared**; ${text.replace(/^Conversation cleared,?\s*/, "")}`,
  // `session/load` fallback after a restart: the model's window is empty.
  context_reset: (text) => `> ⚠️ **Conversation context reset**; ${text}`,
  compacted: (text) => `> ⚠️ **Conversation compacted**; ${text.replace(/^Conversation compacted[;,]?\s*/, "")}`,
  summary: (text) =>
    `> 📝 **Summary of conversation so far**\n>\n${text
      .split("\n")
      .map((line) => `> ${line}`)
      .join("\n")}`,
};

export function activityToThreadMessages(
  rows: readonly ActivityRow[],
  visiblyBusy: boolean,
  showClearedTurns = false,
  todosEnabled = true,
  profile: AgentProfile = DEFAULT_AGENT_PROFILE,
): ThreadMessageLike[] {
  // Turns before the last /clear are forgotten by the model, so they fold by default.
  const start = showClearedTurns ? -1 : lastClearIndex(rows);
  const effectiveRows = start >= 0 ? rows.slice(start) : rows;

  const messages: ThreadMessageLike[] = [];
  let currentAssistant: AssistantBuilder | null = null;
  const flushAssistant = () => {
    if (!currentAssistant) return;
    messages.push(currentAssistant.build(todosEnabled, profile));
    currentAssistant = null;
  };
  const pushUser = (
    row: ActivityRow,
    content: ThreadMessageLike["content"],
    extra: Partial<ThreadMessageLike> = {},
  ) => {
    flushAssistant();
    messages.push({ id: row.id, role: "user", content, ...extra, createdAt: parseDate(row.at) });
  };
  const withCustom = (key: string, value: unknown) => ({ metadata: value ? { custom: { [key]: value } } : undefined });

  for (const row of effectiveRows) {
    const callout = CALLOUTS[row.kind];
    if (callout) {
      flushAssistant();
      messages.push({
        id: `assistant-${row.id}`,
        role: "assistant",
        content: [{ type: "text", text: callout(row.text) }],
        createdAt: parseDate(row.at),
      });
      continue;
    }
    if (row.kind === "user_prompt") {
      // Images become image parts; other attachments show as a labelled line.
      const parts = [
        ...(row.text ? [{ type: "text" as const, text: row.text }] : []),
        ...(row.attachments ?? []).map((att) =>
          att.kind === "image"
            ? { type: "image" as const, image: att.url }
            : { type: "text" as const, text: `📎 ${att.name ?? att.kind} (${att.mimeType})` },
        ),
      ];
      pushUser(row, parts.length > 0 ? parts : [{ type: "text", text: "" }]);
      continue;
    }
    // Structured payloads ride on metadata so the user card renders without parsing text.
    if (row.kind === "elicitation_answered") {
      pushUser(row, [{ type: "text", text: row.text }], withCustom("elicitationAnswers", row.elicitationAnswers));
      continue;
    }
    if (row.kind === "user_diff_comments") {
      pushUser(row, [{ type: "text", text: row.text }], withCustom("diffComments", row.diffComments));
      continue;
    }

    currentAssistant ??= new AssistantBuilder(row.id, row.at);
    if (row.kind === "tool_start" && row.tool) {
      currentAssistant.appendToolCall(row.tool);
    } else if (row.kind === "tool_complete" || row.kind === "tool_error" || row.kind === "tool_stopped") {
      currentAssistant.completeToolCall(
        row.toolCallId ?? row.id.replace(/^(done|stopped)-/, ""),
        row.kind === "tool_error",
        row.kind === "tool_stopped",
        row.text,
        row.at,
        row.asyncSubagent ?? false,
      );
    } else if (row.kind === "empty_output") {
      // A turn with no output (interactive-only slash commands) gets a muted note.
      currentAssistant.appendText(`_${row.text}_`);
    } else if (row.kind !== "thinking") {
      // Thinking shows in the spinner; message and unknown kinds render as text.
      currentAssistant.appendText(row.text);
    }
  }
  flushAssistant();

  const last = messages[messages.length - 1];
  if (visiblyBusy && last?.role === "assistant") {
    messages[messages.length - 1] = { ...last, status: { type: "running" } };
  }
  return messages;
}

type ToolResult = { content: string; endedAt?: string; stopped?: boolean; async?: boolean };

// Loose part shape, cast at build time; the renderer parses argsText itself.
type DraftPart =
  | { type: "text"; text: string }
  | {
      type: "tool-call";
      toolCallId: string;
      toolName: string;
      argsText: string;
      result?: ToolResult;
      isError?: boolean;
    };
type ToolPart = Extract<DraftPart, { type: "tool-call" }>;

class AssistantBuilder {
  private id: string;
  private createdAt?: Date;
  private parts: DraftPart[] = [];

  constructor(id: string, createdAtIso: string) {
    this.id = `assistant-${id}`;
    this.createdAt = parseDate(createdAtIso);
  }

  appendText(text: string) {
    if (!text) return;
    const last = this.parts[this.parts.length - 1];
    if (last && last.type === "text") last.text += text;
    else this.parts.push({ type: "text", text });
  }

  /** assistant-ui parts carry no timestamps or titles, so they travel as namespaced args. */
  appendToolCall(tool: ToolCall) {
    const argsObj = parseJsonObject(tool.args_preview) ?? {};
    if (tool.name) argsObj._aoe_title = tool.name;
    if (tool.started_at) argsObj._aoe_started_at = tool.started_at;
    if (tool.parent_tool_call_id) argsObj._aoe_parent_tool_call_id = tool.parent_tool_call_id;
    // The wire name survives later retitles, so subagent launches stay recognisable.
    if (tool.raw_name) argsObj._aoe_raw_tool_name = tool.raw_name;
    if (tool.memory_recall) argsObj._aoe_memory_recall = tool.memory_recall;
    this.parts.push({
      type: "tool-call",
      toolCallId: tool.id,
      toolName: tool.kind || "other",
      argsText: JSON.stringify(argsObj),
    });
  }

  completeToolCall(
    toolCallId: string,
    isError: boolean,
    stopped: boolean,
    resultText: string,
    endedAt: string,
    async: boolean,
  ) {
    const part = this.parts.find((p): p is ToolPart => p.type === "tool-call" && p.toolCallId === toolCallId);
    if (!part) return;
    part.result = { content: resultText, endedAt, stopped: stopped || undefined, async: async || undefined };
    part.isError = isError || undefined;
  }

  build(todosEnabled: boolean, profile: AgentProfile = DEFAULT_AGENT_PROFILE): ThreadMessageLike {
    const grouped = collapseToolRuns(collapseSubagents(this.parts, profile), todosEnabled);
    return {
      id: this.id,
      role: "assistant",
      content: (grouped.length ? grouped : [{ type: "text", text: "" }]) as ThreadMessageLike["content"],
      createdAt: this.createdAt,
    };
  }
}

function argString(argsText: string, key: string): string | null {
  const v = parseJsonObject(argsText)?.[key];
  return typeof v === "string" && v !== "" ? v : null;
}

function isTodoWriteArgsText(argsText: string, todosEnabled: boolean): boolean {
  if (!todosEnabled) return false;
  const title = parseJsonObject(argsText)?._aoe_title;
  return (typeof title === "string" && title.startsWith("Update TODOs")) || hasTodoArrayArgsText(argsText);
}

/** The verbatim child payload group renderers rebuild cards from. */
const childPayload = (p: ToolPart) => ({
  toolCallId: p.toolCallId,
  toolName: p.toolName,
  argsText: p.argsText,
  result: p.result,
  isError: p.isError,
});

function subagentPart(parent: ToolPart, children: ToolPart[], async?: true): ToolPart {
  return {
    type: "tool-call",
    toolCallId: `subagent-${parent.toolCallId}`,
    toolName: SUBAGENT_TASK_NAME,
    argsText: JSON.stringify({ parent: childPayload(parent), children: children.map(childPayload), async }),
  };
}

/** Fold each subagent into one synthetic part: a parent with inline children
 *  (linked by `_aoe_parent_tool_call_id`), an async launch, or a profile-declared
 *  off-protocol subagent tool. Children whose parent is elsewhere stay put. */
function collapseSubagents(parts: DraftPart[], profile: AgentProfile): DraftPart[] {
  const toolParts = parts.filter((p): p is ToolPart => p.type === "tool-call");
  const presentIds = new Set(toolParts.map((p) => p.toolCallId));
  const childrenByParent = new Map<string, ToolPart[]>();
  for (const p of toolParts) {
    const parentId = argString(p.argsText, "_aoe_parent_tool_call_id");
    if (parentId && presentIds.has(parentId)) {
      childrenByParent.set(parentId, [...(childrenByParent.get(parentId) ?? []), p]);
    }
  }
  const children = new Set([...childrenByParent.values()].flat());
  const out: DraftPart[] = [];
  for (const p of parts) {
    if (p.type !== "tool-call") {
      out.push(p);
    } else if (children.has(p)) {
      continue;
    } else if (p.result?.async) {
      out.push(subagentPart(p, [], true));
    } else if (childrenByParent.has(p.toolCallId)) {
      out.push(subagentPart(p, childrenByParent.get(p.toolCallId)!));
    } else if (isSubagentToolName(argString(p.argsText, "_aoe_raw_tool_name"), profile)) {
      out.push(subagentPart(p, []));
    } else {
      out.push(p);
    }
  }
  return out;
}

/** Fold runs of 3+ consecutive tool calls between text into one group. Group ids
 *  anchor on the first child so a growing run keeps its card and expand state. */
function collapseToolRuns(parts: DraftPart[], todosEnabled: boolean): DraftPart[] {
  const out: DraftPart[] = [];
  let run: ToolPart[] = [];
  const group = (toolName: string, prefix: string) =>
    out.push({
      type: "tool-call",
      toolCallId: `${prefix}-${run[0]!.toolCallId}`,
      toolName,
      argsText: JSON.stringify({ children: run.map(childPayload) }),
    });
  const flushRun = () => {
    const isTodo = (p: ToolPart) => isTodoWriteArgsText(p.argsText, todosEnabled);
    if (run.length >= TOOL_GROUP_MIN_RUN && run.every(isTodo)) {
      group(TODO_GROUP_NAME, "todogroup");
    } else if (
      run.length >= TOOL_GROUP_MIN_RUN &&
      // A todo update among real work, or a subagent card, stays inline.
      !run.some((p) => isTodo(p) || p.toolName === SUBAGENT_TASK_NAME)
    ) {
      group(TOOL_GROUP_NAME, "group");
    } else {
      out.push(...run);
    }
    run = [];
  };
  for (const part of parts) {
    if (part.type === "tool-call") {
      run.push(part);
    } else {
      flushRun();
      out.push(part);
    }
  }
  flushRun();
  return out;
}
