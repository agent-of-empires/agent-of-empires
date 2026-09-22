import { MessagePrimitive, useAuiState } from "@assistant-ui/react";

import { isElicitationAnswersPayload, type ActivityRow, type ToolCall } from "../../lib/acpTypes";
import { parseJsonObject } from "../../lib/acpArgs";
import { pickMemoryRecall } from "../../lib/memoryRecall";
import { ArtifactImage } from "./artifactMedia";
import { DiffCommentsUserCard } from "../diff/comments/DiffCommentsUserCard";
import { isDiffCommentsCardPayload, parseDiffCommentsSentinel } from "../diff/comments/buildPrompt";
import { SUBAGENT_TASK_NAME, TODO_GROUP_NAME, TOOL_GROUP_NAME } from "./activityMessages";
import { ElicitationAnswerCard } from "./ElicitationAnswerCard";
import { Markdown } from "./Markdown";
import { AsyncSubagentCard, SubagentCard, ToolGroupCard } from "./GroupToolCards";
import { TodoGroupCard } from "./TodoCards";
import { ToolCard } from "./ToolCards";

export function UserMessage() {
  return (
    <MessagePrimitive.Root className="group mt-4 flex flex-col items-end gap-1">
      <MessagePrimitive.Parts components={{ Text: UserText, Image: UserImage }} />
    </MessagePrimitive.Root>
  );
}

// A bare <img src> can't carry the auth headers the attachment route requires
// (e.g. the passphrase device-binding header), so route it through the same
// authenticated-fetch-to-blob-URL path artifacts already use.
function UserImage({ image }: { image: string }) {
  return <ArtifactImage url={image} alt="attachment" />;
}

function UserText({ text }: { text: string }) {
  const typedPayload = useAuiState(
    (s) => (s.message.metadata?.custom as { diffComments?: unknown } | undefined)?.diffComments,
  );
  const answers = useAuiState(
    (s) => (s.message.metadata?.custom as { elicitationAnswers?: unknown } | undefined)?.elicitationAnswers,
  );
  if (isDiffCommentsCardPayload(typedPayload)) {
    return <DiffCommentsUserCard payload={typedPayload} />;
  }
  if (isElicitationAnswersPayload(answers)) {
    return <ElicitationAnswerCard answers={answers} />;
  }
  // Older persisted prompts carry the diff-comments payload as a base64 sentinel.
  const payload = parseDiffCommentsSentinel(text);
  if (payload) {
    return <DiffCommentsUserCard payload={payload} />;
  }
  // `breaks`: the composer is a plain textarea, so a single newline must stay visible.
  return (
    <div className="max-w-[80%] min-w-0 rounded-2xl rounded-br-sm border border-surface-700 bg-surface-800/70 px-3 py-1.5 text-sm">
      <Markdown text={text} smooth={false} breaks />
    </div>
  );
}

export function AssistantMessage() {
  return (
    <MessagePrimitive.Root className="group mt-4 mr-auto w-full">
      <div className="text-sm text-text-primary leading-relaxed">
        <MessagePrimitive.Parts components={{ Text: AssistantText, tools: { Override: AssistantToolCall } }} />
      </div>
    </MessagePrimitive.Root>
  );
}

function AssistantText({ text }: { text: string }) {
  // Only the live streaming message smooth-reveals; history renders at once.
  const isRunning = useAuiState((s) => s.message.status?.type === "running");
  if (!text) return null;
  return <Markdown text={text} smooth={isRunning} />;
}

/** assistant-ui tool-call part; AcpRuntime puts `{ content, endedAt?, stopped? }` in `result`. */
interface ToolPart {
  toolCallId: string;
  toolName: string;
  args?: Record<string, unknown>;
  argsText?: string;
  result?: unknown;
  isError?: boolean;
}

// Minted once per tool call so a missing timestamp stays referentially stable across renders.
const TOOL_CALL_TIMES = new Map<string, string>();

function toolCallTimestamp(id: string): string {
  let t = TOOL_CALL_TIMES.get(id);
  if (t === undefined) {
    t = new Date().toISOString();
    TOOL_CALL_TIMES.set(id, t);
  }
  return t;
}

function pickStartedAt(args: Record<string, unknown> | undefined, argsText: string | undefined): string | null {
  for (const o of [args, argsText ? parseJsonObject(argsText) : null]) {
    if (typeof o?._aoe_started_at === "string") return o._aoe_started_at;
  }
  return null;
}

function prettifyToolName(kind: string, args?: Record<string, unknown>): string {
  for (const key of ["_aoe_title", "path", "file_path", "filePath", "command", "cmd", "query", "url"]) {
    const v = args?.[key];
    if (typeof v === "string" && v.length > 0) return v;
  }
  return kind || "tool";
}

function safeStringify(v: unknown): string {
  try {
    return JSON.stringify(v ?? null);
  } catch {
    return "";
  }
}

/** Rebuild the ToolCall and completion row a tool part stands for. Error wins
 *  over stopped; stopped wins over complete. */
function toToolItem(part: ToolPart): { tool: ToolCall; result?: ActivityRow; kind: string } {
  const fallbackAt = toolCallTimestamp(part.toolCallId);
  const res = part.result as { content?: unknown; endedAt?: unknown; stopped?: unknown } | null | undefined;
  const tool: ToolCall = {
    id: part.toolCallId,
    name: prettifyToolName(part.toolName, part.args),
    kind: part.toolName,
    args_preview: part.argsText ?? safeStringify(part.args ?? null),
    started_at: pickStartedAt(part.args, part.argsText) ?? fallbackAt,
    memory_recall: pickMemoryRecall(part.args, part.argsText),
  };
  const result =
    part.result !== undefined
      ? {
          id: `done-${part.toolCallId}`,
          kind: part.isError
            ? ("tool_error" as const)
            : res?.stopped === true
              ? ("tool_stopped" as const)
              : ("tool_complete" as const),
          text: res && typeof res === "object" && "content" in res ? String(res.content ?? "") : "",
          toolCallId: part.toolCallId,
          at: typeof res?.endedAt === "string" ? res.endedAt : fallbackAt,
        }
      : undefined;
  return { tool, result, kind: part.toolName };
}

interface GroupChild {
  toolCallId: string;
  toolName: string;
  argsText: string;
  result?: { content: string; endedAt?: string; stopped?: boolean };
  isError?: boolean;
}

const childItem = (c: GroupChild) => toToolItem({ ...c, args: parseJsonObject(c.argsText) ?? {} });

/** The synthetic group parts AcpRuntime emits carry `{ children, parent?, async? }` in argsText. */
function groupPayload(argsText?: string) {
  const payload = argsText ? parseJsonObject(argsText) : null;
  const children = Array.isArray(payload?.children) ? (payload.children as GroupChild[]) : null;
  return { payload, children };
}

function AssistantToolCall(props: ToolPart) {
  switch (props.toolName) {
    case TOOL_GROUP_NAME:
      return <ToolGroupCard items={(groupPayload(props.argsText).children ?? []).map(childItem)} />;
    case TODO_GROUP_NAME:
      return <TodoGroupCard items={(groupPayload(props.argsText).children ?? []).map(childItem)} />;
    case SUBAGENT_TASK_NAME: {
      const { payload, children } = groupPayload(props.argsText);
      if (!payload?.parent || !children) return null;
      const parent = childItem(payload.parent as GroupChild);
      // An async launch has no inline children; it links to the Background agents panel.
      if (payload.async) return <AsyncSubagentCard tool={parent.tool} />;
      return <SubagentCard tool={parent.tool} result={parent.result} children={children.map(childItem)} />;
    }
    default: {
      const { tool, result } = toToolItem(props);
      return <ToolCard tool={tool} result={result} />;
    }
  }
}
