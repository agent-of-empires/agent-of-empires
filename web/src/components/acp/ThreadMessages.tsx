import { createContext, useContext, useMemo } from "react";

import { MessagePrimitive, useAui, useAuiState } from "@assistant-ui/react";

import { isElicitationAnswersPayload, type ActivityRow, type ToolCall, type ToolOutputBlock } from "../../lib/acpTypes";
import { parseJsonObject } from "../../lib/acpArgs";
import { pickDiffs } from "../../lib/diffPreview";
import { pickMemoryRecall } from "../../lib/memoryRecall";
import { openInNewTab } from "../../lib/openInNewTab";
import { ArtifactImage } from "./artifactMedia";
import { DiffCommentsUserCard } from "../diff/comments/DiffCommentsUserCard";
import { isDiffCommentsCardPayload, parseDiffCommentsSentinel } from "../diff/comments/buildPrompt";
import { SUBAGENT_TASK_NAME, TODO_GROUP_NAME, TOOL_GROUP_NAME } from "./activityMessages";
import { ElicitationAnswerCard } from "./ElicitationAnswerCard";
import { CopyMarkdownButton } from "./CopyMarkdownButton";
import { Markdown } from "./Markdown";
import { MessageCopyMenu } from "./MessageCopyMenu";
import { AsyncSubagentCard, SubagentCard, ToolGroupCard } from "./GroupToolCards";
import { TodoGroupCard } from "./TodoCards";
import { ToolCard } from "./ToolCards";

// True while the parts render inside one shared bubble, so text drops its own.
const SharedBubbleContext = createContext(false);

export function UserMessage() {
  const originPlugin = useAuiState(
    (s) => (s.message.metadata?.custom as { originPlugin?: unknown } | undefined)?.originPlugin,
  );
  // Cards (diff comments, elicitation answers) bring their own chrome.
  const shareBubble = useAuiState((s) => {
    const custom = s.message.metadata?.custom as { diffComments?: unknown; elicitationAnswers?: unknown } | undefined;
    return (
      !custom?.diffComments &&
      !custom?.elicitationAnswers &&
      s.message.content.some((part) => part.type === "image" || part.type === "file")
    );
  });
  // The text parts only: an attachment has nothing to copy as markdown.
  const promptText = useAuiState((s) =>
    s.message.content.flatMap((part) => (part.type === "text" ? [part.text] : [])).join("\n\n"),
  );
  const parts = <MessagePrimitive.Parts components={{ Text: UserText, Image: UserImage, File: UserFile }} />;
  return (
    <MessagePrimitive.Root className="group mt-4 flex flex-col items-end gap-1">
      {typeof originPlugin === "string" && (
        <div data-testid="user-message-origin" className="text-xs text-text-dim">
          Sent by plugin {originPlugin}
        </div>
      )}
      {shareBubble ? (
        <div className="acp-copy-host flex max-w-[80%] min-w-0 flex-col items-end">
          <div
            data-testid="user-message-bubble"
            className="flex min-w-0 flex-col gap-1.5 rounded-2xl rounded-br-sm border border-surface-700 bg-surface-800/70 p-1.5 text-sm"
          >
            <SharedBubbleContext.Provider value>{parts}</SharedBubbleContext.Provider>
          </div>
          {promptText.trim() && (
            <div className="acp-copy-reveal">
              <CopyMarkdownButton compact source={promptText} label="Copy prompt" />
            </div>
          )}
        </div>
      ) : (
        parts
      )}
    </MessagePrimitive.Root>
  );
}

// A bare <img src> can't carry the auth headers the attachment route requires
// (e.g. the passphrase device-binding header), so route it through the same
// authenticated-fetch-to-blob-URL path artifacts already use.
function UserImage({ image }: { image: string }) {
  return <ArtifactImage url={image} alt="attachment" expandable />;
}

// Documents are not rendered; they open in a new tab through the authenticated fetch.
function UserFile({ data, filename, mimeType }: { data: string; filename?: string; mimeType: string }) {
  return (
    <button
      type="button"
      title={`Open ${mimeType}`}
      className="flex max-w-full cursor-pointer items-center gap-1.5 self-end rounded-md border border-surface-700 bg-surface-900/60 px-2 py-1 text-left text-xs text-text-secondary hover:text-text-primary"
      onClick={() => void openInNewTab(data, filename)}
    >
      <span aria-hidden>📎</span>
      <span className="min-w-0 truncate">{filename ?? "attachment"}</span>
    </button>
  );
}

function UserText({ text }: { text: string }) {
  const inSharedBubble = useContext(SharedBubbleContext);
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
  if (inSharedBubble) {
    return (
      <div className="min-w-0 px-1.5 py-0.5">
        <Markdown text={text} smooth={false} breaks />
      </div>
    );
  }
  return (
    <div className="acp-copy-host flex max-w-[80%] min-w-0 flex-col items-end">
      <div className="min-w-0 rounded-2xl rounded-br-sm border border-surface-700 bg-surface-800/70 px-3 py-1.5 text-sm">
        <Markdown text={text} smooth={false} breaks />
      </div>
      <div className="acp-copy-reveal">
        <CopyMarkdownButton compact source={text} label="Copy prompt" />
      </div>
    </div>
  );
}

export function AssistantMessage() {
  return (
    <MessagePrimitive.Root className="acp-copy-host group mt-4 mr-auto w-full">
      <div className="text-sm text-text-primary leading-relaxed">
        <MessagePrimitive.Parts components={{ Text: AssistantText, tools: { Override: AssistantToolCall } }} />
      </div>
      <MessageCopyMenu />
    </MessagePrimitive.Root>
  );
}

function AssistantText({ text }: { text: string }) {
  // Only the live streaming message smooth-reveals; history renders at once.
  const isRunning = useAuiState((s) => s.message.status?.type === "running");
  const compactionSummary = useAuiState(
    (s) => (s.message.metadata?.custom as { compactionSummary?: unknown } | undefined)?.compactionSummary === true,
  );
  // A lone part is the whole message, and the last part's button lives in the message footer.
  const query = useAui().part.query as { index?: number } | null;
  const partIndex = query?.index ?? -1;
  const ownsFooter = useAuiState((s) => partIndex >= 0 && partIndex < s.message.content.length - 1);
  if (!text) return null;
  if (compactionSummary) {
    return (
      <details className="rounded border border-surface-800 bg-surface-900/40">
        <summary className="cursor-pointer select-none px-3 py-1 text-text-dim hover:text-text-secondary">
          📝 Compaction summary
        </summary>
        <div className="px-3 pb-2">
          <Markdown text={text} smooth={false} />
        </div>
      </details>
    );
  }
  return (
    <div className="acp-copy-host">
      <Markdown text={text} smooth={isRunning} />
      {ownsFooter && (
        <div className="acp-copy-reveal flex justify-end">
          <CopyMarkdownButton source={text} label="Copy text as markdown" compact />
        </div>
      )}
    </div>
  );
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
  const res = part.result as
    | { content?: unknown; endedAt?: unknown; stopped?: unknown; output?: ToolOutputBlock[] }
    | null
    | undefined;
  const tool: ToolCall = {
    id: part.toolCallId,
    name: prettifyToolName(part.toolName, part.args),
    kind: part.toolName,
    args_preview: part.argsText ?? safeStringify(part.args ?? null),
    started_at: pickStartedAt(part.args, part.argsText) ?? fallbackAt,
    memory_recall: pickMemoryRecall(part.args, part.argsText),
    diffs: pickDiffs(part.args, part.argsText),
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
          output: res?.output,
        }
      : undefined;
  return { tool, result, kind: part.toolName };
}

interface GroupChild {
  toolCallId: string;
  toolName: string;
  argsText: string;
  result?: { content: string; endedAt?: string; stopped?: boolean; output?: ToolOutputBlock[] };
  isError?: boolean;
}

const childItem = (c: GroupChild) => toToolItem({ ...c, args: parseJsonObject(c.argsText) ?? {} });

/** The synthetic group parts AcpRuntime emits carry `{ children, parent?, async? }` in argsText.
 *  Memoized per payload so a re-render does not re-parse every child. */
function useGroupPayload(argsText?: string) {
  return useMemo(() => {
    const payload = argsText ? parseJsonObject(argsText) : null;
    const children = Array.isArray(payload?.children) ? (payload.children as GroupChild[]).map(childItem) : null;
    const parent = payload?.parent ? childItem(payload.parent as GroupChild) : null;
    return { payload, children, parent };
  }, [argsText]);
}

function AssistantToolGroup({ argsText }: ToolPart) {
  return <ToolGroupCard items={useGroupPayload(argsText).children ?? []} />;
}

function AssistantTodoGroup({ argsText }: ToolPart) {
  return <TodoGroupCard items={useGroupPayload(argsText).children ?? []} />;
}

function AssistantSubagent({ argsText }: ToolPart) {
  const { payload, children, parent } = useGroupPayload(argsText);
  if (!parent || !children) return null;
  // An async launch has no inline children; it links to the Background agents panel.
  if (payload?.async) return <AsyncSubagentCard tool={parent.tool} />;
  return <SubagentCard tool={parent.tool} result={parent.result} children={children} />;
}

function AssistantToolCall(props: ToolPart) {
  switch (props.toolName) {
    case TOOL_GROUP_NAME:
      return <AssistantToolGroup {...props} />;
    case TODO_GROUP_NAME:
      return <AssistantTodoGroup {...props} />;
    case SUBAGENT_TASK_NAME:
      return <AssistantSubagent {...props} />;
    default: {
      const { tool, result } = toToolItem(props);
      return <ToolCard tool={tool} result={result} />;
    }
  }
}
