/* eslint-disable react-refresh/only-export-components */
// Bridge between our `useStructuredView` state (which subscribes to the
// acp WebSocket) and assistant-ui's external-store runtime. The
// runtime adapter is the seam: assistant-ui owns the chat surface
// (rendering, scrolling, accessibility, message editing affordances);
// we own the data (events from the ACP-driven supervisor) and the
// actions (sendPrompt, cancelPrompt, resolveApproval).
//
// Flow:
//   ws frame  ─►  applyEvent → AcpState.activity (ours)
//                                      │
//                                      ▼
//                       activityToThreadMessages()  ────►  ThreadMessageLike[]
//                                      │
//                                      ▼
//                       useExternalStoreRuntime(adapter) ───►  AssistantRuntime
//                                      │
//                                      ▼
//                       <AssistantRuntimeProvider runtime>
//                                      │
//                       ▼              │              ▼
//          <ThreadPrimitive.Messages>  │   <ComposerPrimitive.Root>
//                                      │
//                       onNew: sendPrompt   onCancel: cancelPrompt
//
// We keep all of our existing renderers (Markdown, ToolCards, the
// rattle spinner, ApprovalCard) and slot them into assistant-ui's
// component-injection points.

import { AssistantRuntimeProvider, useExternalStoreRuntime, type ThreadMessageLike } from "@assistant-ui/react";
import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";

import { useAcpSession } from "../../hooks/useAcpSession";
import type {
  ActivityRow,
  ApprovalDecision,
  AcpState,
  ElicitationResolution,
  PromptAttachmentInput,
  ToolCall,
} from "../../lib/acpTypes";
import { hasTodoArrayArgsText, parseJsonObject } from "../../lib/acpArgs";
import { getDraftAttachments, setDraftAttachments } from "../../lib/acpDrafts";
import { useHistoryWindow } from "../../hooks/useHistoryWindow";
import { canOfferEarlier, earlierAction } from "../../lib/historyScroll";
import { useAgentProfile } from "../../lib/agentProfileContext";
import { useCancelEscalation } from "./useCancelEscalation";

// Re-exported for existing tests that import it from this module; the
// implementation now lives alongside the escalation hook. See #2237.
export { nextCancelAction } from "./useCancelEscalation";

interface Props {
  sessionId: string;
  /** Live acp worker lifecycle pulled from `SessionResponse.acp_worker_state`.
   *  Threaded through to `useStructuredView` so the drain effect parks queued
   *  prompts while the reconciler is mid-resume. See #1088. */
  acpWorkerState?: "absent" | "resuming" | "running";
  /** RFC3339 archived-at timestamp, or null. Threaded into `useStructuredView`
   *  so `sendPrompt` can auto-unarchive the session before enqueueing,
   *  matching the `touch_last_accessed` invariant the server enforces
   *  for tmux sends. See #1581. */
  archivedAt?: string | null;
  /** RFC3339 snoozed-until timestamp, or null. Same auto-wake purpose
   *  as `archivedAt`. See #1581. */
  snoozedUntil?: string | null;
  /** When true, every row is rendered including those preceding the
   *  most recent `/clear`. When false (the default), rows before the
   *  latest `session_cleared` divider are folded out of the message
   *  tree so the user doesn't reply on top of a transcript the model
   *  has forgotten. The `ClearedTurnsBanner` in `StructuredView` provides
   *  the toggle. See #1101. */
  showClearedTurns?: boolean;
  children: (ctx: AcpContext) => ReactNode;
}

export interface AcpContext {
  state: AcpState;
  status: ReturnType<typeof useAcpSession>["status"];
  hasEverOpened: boolean;
  /** True while the auto-reconnect backoff is armed between a close
   *  and the next dial. Drives the "Reconnecting (N/MAX) in Xs" copy
   *  in SystemNotices. See #1130. */
  reconnecting: boolean;
  retryCount: number;
  retryCountdown: number;
  maxRetries: number;
  manualReconnect: () => void;
  resolveApproval: (nonce: string, decision: ApprovalDecision) => Promise<void>;
  resolveElicitation: (nonce: string, resolution: ElicitationResolution) => Promise<void>;
  sendPrompt: (text: string, attachments?: PromptAttachmentInput[]) => Promise<void>;
  /** Attachments the composer has staged for the next send. Owned here
   *  (above the assistant-ui runtime) so `onNew` can attach them when
   *  the user submits via Enter / the assistant-ui Send path, and the
   *  composer can render + clear them. See #1000 / #965. */
  pendingAttachments: PromptAttachmentInput[];
  setPendingAttachments: React.Dispatch<React.SetStateAction<PromptAttachmentInput[]>>;
  forceEndTurn: () => Promise<void>;
  lastActivityRef: ReturnType<typeof useAcpSession>["lastActivityRef"];
  dismissError: () => void;
  dismissPrimer: () => void;
  removeQueuedPrompt: (id: string) => void;
  editQueuedPrompt: (id: string, text: string) => void;
  clearQueue: () => void;
  dismissRejectedPrompt: (id: string) => void;
  dismissModeSwitchFailed: () => void;
  setConfigOption: (configId: string, value: string) => Promise<void>;
  dismissConfigOptionSwitchFailed: () => void;
  /** True when older rows exist above the rendered window, either already
   *  in the reducer (client window) or still on the server (recent-first
   *  paging), so the view can offer a "Load earlier" control. See #2236. */
  canLoadEarlierHistory: boolean;
  /** Reveal more older history: first already-loaded rows, then fetch the
   *  next-older page from the server once those run out. See #2236. */
  loadEarlierHistory: () => void;
  /** True while an older-history page fetch is in flight, for a spinner
   *  on the "Load earlier" affordance. See #2236. */
  loadingEarlierHistory: boolean;
}

/**
 * Wraps children in an `<AssistantRuntimeProvider>` driven by our
 * acp WS state. Children get a render-prop callback with the raw
 * acp state + actions for things assistant-ui doesn't own
 * (approvals, plan strip, system notices).
 */
export function AcpRuntime({
  sessionId,
  acpWorkerState = "running",
  archivedAt = null,
  snoozedUntil = null,
  showClearedTurns = false,
  children,
}: Props) {
  const acp = useAcpSession(sessionId, acpWorkerState, archivedAt, snoozedUntil);
  const agentProfile = useAgentProfile();
  // Staged attachments for the next prompt. A ref mirror keeps `onNew`
  // (recreated each render by useExternalStoreRuntime) reading the
  // latest value without going stale. See #1000 / #965.
  // Seed from the persisted draft so a staged image survives a session
  // switch or reload like unsent text does. StructuredView remounts this
  // runtime per session (`key={sessionId}`), so the initializer runs once
  // with the right session's attachments and `sessionId` is stable for the
  // instance lifetime, which keeps the persist effect below race-free.
  const [pendingAttachments, setPendingAttachments] = useState<PromptAttachmentInput[]>(() =>
    getDraftAttachments(sessionId),
  );
  const pendingAttachmentsRef = useRef<PromptAttachmentInput[]>(pendingAttachments);
  useEffect(() => {
    pendingAttachmentsRef.current = pendingAttachments;
  }, [pendingAttachments]);
  // Mirror staged attachments into the per-session draft on every change.
  // Skip the first run: the state was just initialized from storage, so
  // re-serializing (potentially megabytes of base64) straight back would
  // be wasted main-thread work on every session open. Post-send the
  // composer clears pendingAttachments, which removes the key here.
  const attachmentsHydratedRef = useRef(false);
  useEffect(() => {
    if (!attachmentsHydratedRef.current) {
      attachmentsHydratedRef.current = true;
      return;
    }
    setDraftAttachments(sessionId, pendingAttachments);
  }, [sessionId, pendingAttachments]);
  // Stop-button escalation: a second press always force-ends, even when the
  // server never confirms the first cancel (no in-flight prompt on the
  // daemon). Owns its own reset-on-turn-end and reset-on-session-switch
  // lifecycle. See #2237.
  const onCancel = useCancelEscalation(
    sessionId,
    acp.state.pendingUserPromptSeq,
    acp.state.cancelling,
    acp.cancelPrompt,
    acp.forceEndTurn,
  );
  // Render only the most recent slice of the transcript so a long
  // session does not block first paint on mobile; older rows stay in
  // reducer state and are revealed via "Load earlier". See #2144.
  const { windowedActivity, canLoadEarlier, loadEarlier } = useHistoryWindow(
    sessionId,
    acp.state.activity,
    showClearedTurns,
  );
  // "Load earlier" is two-stage: first reveal rows already loaded into
  // the reducer (client window), then, once those are exhausted, fetch
  // the next-older page from the server (recent-first paging). One
  // affordance, no competing mechanisms. See #2236.
  const { loadOlder, hasMoreOlder, loadingOlder } = acp;
  const canLoadEarlierHistory = canOfferEarlier(canLoadEarlier, hasMoreOlder);
  const loadEarlierHistory = useCallback(() => {
    const action = earlierAction(canLoadEarlier, hasMoreOlder);
    if (action === "reveal") loadEarlier();
    else if (action === "fetch") void loadOlder();
  }, [canLoadEarlier, hasMoreOlder, loadEarlier, loadOlder]);

  // Memoise the activity → ThreadMessageLike conversion. The function
  // walks the activity array, allocates a new AssistantBuilder
  // per turn, and produces brand-new message objects. Without
  // useMemo, every parent re-render (e.g. WS heartbeat, hover state)
  // re-builds the transcript and assistant-ui treats every
  // message as changed. Memo on the inputs the function reads.
  const messages = useMemo(
    () =>
      activityToThreadMessages(
        windowedActivity,
        acp.state.turnActive,
        showClearedTurns,
        agentProfile.capabilities.todos,
      ),
    [windowedActivity, acp.state.turnActive, showClearedTurns, agentProfile.capabilities.todos],
  );

  const runtime = useExternalStoreRuntime<ThreadMessageLike>({
    messages,
    isRunning: acp.state.turnActive,
    convertMessage: (m) => m,
    onNew: async (msg) => {
      // assistant-ui hands us an AppendMessage with mixed parts. Flatten
      // text parts into one string; the composer stages attachments
      // separately (assistant-ui's own attachment system is unused), so
      // pull them from the pending ref and clear it on send. See #1000.
      const text = msg.content
        .map((c) => (c.type === "text" ? c.text : ""))
        .join("")
        .trim();
      const attachments = [...pendingAttachmentsRef.current];
      if (!text && attachments.length === 0) return;
      // Clear staged attachments only after the send resolves, so a
      // failed send keeps them staged for retry instead of dropping them.
      await acp.sendPrompt(text, attachments);
      // Drop the persisted draft synchronously here, not only via the
      // pendingAttachments effect: a send-then-immediately-navigate-away
      // can unmount before the post-send render commits, which would
      // otherwise leave an already-sent image behind to rehydrate later.
      pendingAttachmentsRef.current = [];
      setDraftAttachments(sessionId, []);
      setPendingAttachments([]);
    },
    onCancel,
  });

  return (
    <AssistantRuntimeProvider runtime={runtime}>
      {children({
        state: acp.state,
        status: acp.status,
        hasEverOpened: acp.hasEverOpened,
        reconnecting: acp.reconnecting,
        retryCount: acp.retryCount,
        retryCountdown: acp.retryCountdown,
        maxRetries: acp.maxRetries,
        manualReconnect: acp.manualReconnect,
        resolveApproval: acp.resolveApproval,
        resolveElicitation: acp.resolveElicitation,
        sendPrompt: acp.sendPrompt,
        pendingAttachments,
        setPendingAttachments,
        forceEndTurn: acp.forceEndTurn,
        lastActivityRef: acp.lastActivityRef,
        dismissError: acp.dismissError,
        dismissPrimer: acp.dismissPrimer,
        removeQueuedPrompt: acp.removeQueuedPrompt,
        editQueuedPrompt: acp.editQueuedPrompt,
        clearQueue: acp.clearQueue,
        dismissRejectedPrompt: acp.dismissRejectedPrompt,
        dismissModeSwitchFailed: acp.dismissModeSwitchFailed,
        setConfigOption: acp.setConfigOption,
        dismissConfigOptionSwitchFailed: acp.dismissConfigOptionSwitchFailed,
        canLoadEarlierHistory,
        loadEarlierHistory,
        loadingEarlierHistory: loadingOlder,
      })}
    </AssistantRuntimeProvider>
  );
}

/**
 * Convert the flat `ActivityRow` log into the message tree assistant-ui
 * expects. Each `user_prompt` opens a new user message; subsequent
 * agent rows (text chunks + tool calls) collapse into one assistant
 * message until the next user_prompt or end of log.
 *
 * Tool completion rows (`tool_complete` / `tool_error`) are not their
 * own messages; they update the matching `tool-call` part in place
 * by setting `result` / `isError`, so the per-tool card renderer can
 * render running → done in one place.
 */
export function activityToThreadMessages(
  rows: readonly ActivityRow[],
  turnActive: boolean,
  showClearedTurns = false,
  todosEnabled = true,
): ThreadMessageLike[] {
  // Fold pre-clear turns by default. When the user has run `/clear`,
  // earlier rows describe a conversation the model has forgotten; the
  // banner in StructuredView surfaces a count + "show" toggle that lifts
  // `showClearedTurns` to true. We pin to the LAST clear so multiple
  // /clears collapse cumulatively. See #1101.
  let effectiveRows: readonly ActivityRow[] = rows;
  if (!showClearedTurns) {
    let lastClearIndex = -1;
    for (let i = rows.length - 1; i >= 0; i -= 1) {
      if (rows[i]!.kind === "session_cleared") {
        lastClearIndex = i;
        break;
      }
    }
    if (lastClearIndex >= 0) {
      effectiveRows = rows.slice(lastClearIndex);
    }
  }

  const messages: ThreadMessageLike[] = [];
  let currentAssistant: AssistantBuilder | null = null;

  const flushAssistant = () => {
    if (!currentAssistant) return;
    messages.push(currentAssistant.build(todosEnabled));
    currentAssistant = null;
  };

  for (const row of effectiveRows) {
    if (row.kind === "session_cleared") {
      flushAssistant();
      messages.push({
        id: `assistant-${row.id}`,
        role: "assistant",
        content: [
          {
            type: "text",
            text: `> ⚠️ **Conversation cleared**; ${row.text.replace(/^Conversation cleared,?\s*/, "")}`,
          },
        ],
        createdAt: parseDate(row.at),
      });
      continue;
    }
    if (row.kind === "user_prompt") {
      flushAssistant();
      // `ThreadMessageLike["content"]` is a readonly array, so build the
      // parts via spreads rather than push. Images become image parts;
      // audio / embedded resources have no inline player here yet, so
      // surface them as a labelled chip line.
      const parts = [
        ...(row.text ? [{ type: "text" as const, text: row.text }] : []),
        ...(row.attachments ?? []).map((att) =>
          att.kind === "image"
            ? { type: "image" as const, image: att.url }
            : {
                type: "text" as const,
                text: `📎 ${att.name ?? att.kind} (${att.mimeType})`,
              },
        ),
      ];
      messages.push({
        id: row.id,
        role: "user",
        content: parts.length > 0 ? parts : [{ type: "text", text: "" }],
        createdAt: parseDate(row.at),
      });
      continue;
    }

    if (row.kind === "elicitation_answered") {
      // The user's answer to an AskUserQuestion / elicitation form. Render
      // as a user-authored message so the picked answer reads like the
      // user's turn (mirrors how Claude Code shows it), but keep it a
      // distinct row kind so it never folds into prompt grouping. The
      // structured pairs ride on metadata for richer rendering. See #2209.
      flushAssistant();
      messages.push({
        id: row.id,
        role: "user",
        content: [{ type: "text", text: row.text }],
        metadata: row.elicitationAnswers ? { custom: { elicitationAnswers: row.elicitationAnswers } } : undefined,
        createdAt: parseDate(row.at),
      });
      continue;
    }

    if (row.kind === "user_diff_comments") {
      // A typed diff-comments prompt. The assembled markdown is the
      // user-visible / agent body (and the fallback if the card can't
      // render); the structured payload rides on the message metadata
      // so UserText can render the rich DiffCommentsUserCard without
      // parsing any sentinel. See #1123.
      flushAssistant();
      messages.push({
        id: row.id,
        role: "user",
        content: [{ type: "text", text: row.text }],
        metadata: row.diffComments ? { custom: { diffComments: row.diffComments } } : undefined,
        createdAt: parseDate(row.at),
      });
      continue;
    }

    if (row.kind === "context_reset") {
      // `session/load` fallback after an `aoe serve` restart: model's
      // window is empty even though we replay the prior transcript.
      // Renders the amber-callout divider; the parallel
      // ContextPrimerBanner offers the recovery affordance.
      flushAssistant();
      messages.push({
        id: `assistant-${row.id}`,
        role: "assistant",
        content: [
          {
            type: "text",
            text: `> ⚠️ **Conversation context reset**; ${row.text}`,
          },
        ],
        createdAt: parseDate(row.at),
      });
      continue;
    }

    if (row.kind === "compacted") {
      // `/compact` completion: the model's window has been replaced
      // by a summary while the rendered transcript stays put. Same
      // amber-callout shape as a true context reset, different
      // header. No primer banner fires (see #1109); the model still
      // has continuity through the summary.
      flushAssistant();
      const body = row.text.replace(/^Conversation compacted[;,]?\s*/, "");
      messages.push({
        id: `assistant-${row.id}`,
        role: "assistant",
        content: [
          {
            type: "text",
            text: `> ⚠️ **Conversation compacted**; ${body}`,
          },
        ],
        createdAt: parseDate(row.at),
      });
      continue;
    }

    if (row.kind === "summary") {
      // aoe-generated recap of the conversation so far (see #2808). A
      // blockquote callout with the summary body; distinct header from
      // the compaction divider since this is our own summary, not the
      // model's context replacement.
      flushAssistant();
      messages.push({
        id: `assistant-${row.id}`,
        role: "assistant",
        content: [
          {
            type: "text",
            text: `> 📝 **Summary of conversation so far**\n>\n${row.text
              .split("\n")
              .map((line) => `> ${line}`)
              .join("\n")}`,
          },
        ],
        createdAt: parseDate(row.at),
      });
      continue;
    }

    if (!currentAssistant) {
      currentAssistant = new AssistantBuilder(row.id, row.at);
    }

    if (row.kind === "message") {
      currentAssistant.appendText(row.text);
    } else if (row.kind === "tool_start" && row.tool) {
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
    } else if (row.kind === "thinking") {
      // Thinking is rendered by the global rattle spinner, not the
      // message stream.
    } else if (row.kind === "empty_output") {
      // Synthesised when the agent finished a turn without emitting any
      // text or tool calls (e.g. interactive-only slash commands like
      // /usage, /status, /memory in claude-agent-acp; see upstream
      // issue agentclientprotocol/claude-agent-acp#642). Surface it as
      // a tiny muted notice instead of leaving the assistant bubble
      // empty.
      currentAssistant.appendText(`_${row.text}_`);
    } else {
      // Unknown kind: surface as a tiny text part so we don't lose
      // the data, but don't make it the whole message.
      currentAssistant.appendText(row.text);
    }
  }
  flushAssistant();

  // While the agent is still working, leave the last assistant message
  // marked as "running" so assistant-ui knows to keep its skeleton/
  // status indicators alive. The runtime's isRunning prop covers the
  // global flag; per-message status is derived from the trailing
  // message's `status`.
  if (turnActive) {
    const last = messages[messages.length - 1];
    if (last && last.role === "assistant") {
      messages[messages.length - 1] = {
        ...last,
        status: { type: "running" },
      };
    }
  }

  return messages;
}

function parseDate(iso: string): Date | undefined {
  const d = new Date(iso);
  return Number.isFinite(d.getTime()) ? d : undefined;
}

// assistant-ui's `tool-call` content part has its own (readonly,
// JSON-only) shape. We model our parts loosely here and cast at build
// time; the runtime only inspects fields it knows about and our
// per-tool renderer (ToolCards.tsx) reads the rest off `argsText`.
type DraftPart =
  | { type: "text"; text: string }
  | {
      type: "tool-call";
      toolCallId: string;
      toolName: string;
      argsText: string;
      result?: { content: string; endedAt?: string; stopped?: boolean; async?: boolean };
      isError?: boolean;
    };

/** Mutable builder for an assistant message under construction. */
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
    if (last && last.type === "text") {
      last.text += text;
    } else {
      this.parts.push({ type: "text", text });
    }
  }

  appendToolCall(tool: ToolCall) {
    // Forward the ACP tool title alongside the args so per-kind
    // renderers can show a descriptive label when raw_input is
    // empty (Claude's bash tool, for example, often emits an empty
    // raw_input on the initial tool_call frame). The `_aoe_title`
    // key is namespaced so it can't collide with real tool args.
    //
    // Also smuggle `_aoe_started_at` (the real ToolCall.started_at)
    // through assistant-ui's tool-call part shape; its primitive
    // doesn't carry timestamps and StructuredView's fallback would
    // otherwise mint one fresh per render, breaking the duration
    // label (#1060).
    let argsObj: Record<string, unknown> = {};
    try {
      const parsed = JSON.parse(tool.args_preview);
      if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
        argsObj = parsed as Record<string, unknown>;
      }
    } catch {
      // args_preview wasn't a JSON object; keep argsObj empty.
    }
    if (tool.name) argsObj._aoe_title = tool.name;
    if (tool.started_at) argsObj._aoe_started_at = tool.started_at;
    if (tool.parent_tool_call_id) {
      argsObj._aoe_parent_tool_call_id = tool.parent_tool_call_id;
    }
    // Smuggle the structured memory-recall payload so StructuredView can
    // rebuild it onto the reconstructed ToolCall; without this the
    // synthesize/recall card is unreachable through the assistant-ui
    // part shape and falls back to a generic read card. See #2142.
    if (tool.memory_recall) {
      argsObj._aoe_memory_recall = tool.memory_recall;
    }
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
    for (const part of this.parts) {
      if (part.type === "tool-call" && part.toolCallId === toolCallId) {
        part.result = {
          content: resultText,
          endedAt,
          stopped: stopped || undefined,
          async: async || undefined,
        };
        part.isError = isError || undefined;
        return;
      }
    }
  }

  build(todosEnabled: boolean): ThreadMessageLike {
    const subagentCollapsed = collapseSubagents(this.parts);
    const grouped = collapseToolRuns(subagentCollapsed, todosEnabled);
    return {
      id: this.id,
      role: "assistant",
      // Cast to bypass assistant-ui's strict ReadonlyJSONObject typing
      // for tool-call args. We don't carry parsed args through this
      // path; the renderer parses argsText itself; so the loose
      // shape is safe in practice.
      content: (grouped.length ? grouped : [{ type: "text", text: "" }]) as ThreadMessageLike["content"],
      createdAt: this.createdAt,
    };
  }
}

function isTodoWriteArgsText(argsText: string, todosEnabled: boolean): boolean {
  if (!todosEnabled) return false;
  const parsed = parseJsonObject(argsText);
  if (parsed) {
    const title = parsed._aoe_title;
    if (typeof title === "string" && title.startsWith("Update TODOs")) {
      return true;
    }
  }
  return hasTodoArrayArgsText(argsText);
}

/** Synthetic toolName for a Claude sub-agent (Task) and its child tool
 *  calls collapsed into one renderable part. See #1041 layer B. */
export const SUBAGENT_TASK_NAME = "_aoe_subagent_task";

/** Read the smuggled `_aoe_parent_tool_call_id` out of a tool-call
 *  part's argsText. Returns the parent's tool_call_id when the part
 *  represents a sub-agent child tool call; null for top-level calls. */
function parentIdFromArgsText(argsText: string): string | null {
  try {
    const p = JSON.parse(argsText);
    if (p && typeof p === "object" && !Array.isArray(p)) {
      const v = (p as Record<string, unknown>)._aoe_parent_tool_call_id;
      if (typeof v === "string" && v !== "") return v;
    }
  } catch {
    // ignore
  }
  return null;
}

/** Walk an assistant message's parts and collapse each parent-Task
 *  tool call plus its children (matched via `_aoe_parent_tool_call_id`)
 *  into one synthetic `_aoe_subagent_task` part. Children whose parent
 *  is not in the same message are left in place, falling through to
 *  the orphan rendering. Run before `collapseToolRuns` so a parent
 *  Task with N children doesn't get folded into the generic group
 *  card. */
function collapseSubagents(parts: DraftPart[]): DraftPart[] {
  // Identify children + map child-index → parentToolCallId.
  const childToParent = new Map<number, string>();
  for (let i = 0; i < parts.length; i++) {
    const p = parts[i];
    if (!p || p.type !== "tool-call") continue;
    const parentId = parentIdFromArgsText(p.argsText);
    if (parentId) childToParent.set(i, parentId);
  }
  // Async sub-agent launches (Claude `Task` with isAsync) complete
  // immediately and emit NO inline children: the real work runs
  // off-protocol and never reports back on this stream. They carry
  // `result.async` (forwarded from the backend `async_subagent` flag).
  // Render them as a childless sub-agent card so they don't fall
  // through to a generic tool card that leaks the launch marker body.
  const asyncParents = new Set<number>();
  for (let i = 0; i < parts.length; i++) {
    const p = parts[i];
    if (p && p.type === "tool-call" && p.result?.async) asyncParents.add(i);
  }
  if (childToParent.size === 0 && asyncParents.size === 0) return parts;

  // Map each parentId to its part index (only when the parent is in
  // this same message; orphans skip the collapse).
  const referencedParents = new Set(childToParent.values());
  const parentIndex = new Map<string, number>();
  for (let i = 0; i < parts.length; i++) {
    const p = parts[i];
    if (!p || p.type !== "tool-call") continue;
    if (referencedParents.has(p.toolCallId)) parentIndex.set(p.toolCallId, i);
  }

  // Group children by parent (only when parent is present).
  const childrenByParent = new Map<string, DraftPart[]>();
  const childIndicesToDrop = new Set<number>();
  for (const [idx, parentId] of childToParent) {
    if (!parentIndex.has(parentId)) continue;
    const arr = childrenByParent.get(parentId) ?? [];
    const child = parts[idx];
    if (child) arr.push(child);
    childrenByParent.set(parentId, arr);
    childIndicesToDrop.add(idx);
  }
  if (childrenByParent.size === 0 && asyncParents.size === 0) return parts;

  const out: DraftPart[] = [];
  for (let i = 0; i < parts.length; i++) {
    if (childIndicesToDrop.has(i)) continue;
    const p = parts[i];
    if (!p) continue;
    // Async sub-agent: a childless `Task` launch. Emit the synthetic
    // subagent part with no children and async:true so SubagentCard
    // renders a neutral "runs in background" card. Checked before the
    // children branch since an async launch never has inline children.
    if (p.type === "tool-call" && asyncParents.has(i)) {
      out.push({
        type: "tool-call",
        toolCallId: `subagent-${p.toolCallId}`,
        toolName: SUBAGENT_TASK_NAME,
        argsText: JSON.stringify({
          parent: {
            toolCallId: p.toolCallId,
            toolName: p.toolName,
            argsText: p.argsText,
            result: p.result,
            isError: p.isError,
          },
          children: [],
          async: true,
        }),
      });
    } else if (p.type === "tool-call" && parentIndex.has(p.toolCallId)) {
      const childParts = childrenByParent.get(p.toolCallId) ?? [];
      const children = childParts
        .map((c) =>
          c.type === "tool-call"
            ? {
                toolCallId: c.toolCallId,
                toolName: c.toolName,
                argsText: c.argsText,
                result: c.result,
                isError: c.isError,
              }
            : null,
        )
        .filter((c): c is NonNullable<typeof c> => c !== null);
      out.push({
        type: "tool-call",
        toolCallId: `subagent-${p.toolCallId}`,
        toolName: SUBAGENT_TASK_NAME,
        argsText: JSON.stringify({
          parent: {
            toolCallId: p.toolCallId,
            toolName: p.toolName,
            argsText: p.argsText,
            result: p.result,
            isError: p.isError,
          },
          children,
        }),
      });
    } else {
      out.push(p);
    }
  }
  return out;
}

/** Minimum run length that triggers grouping. Two-in-a-row stays inline
 *  so a quick read-then-edit doesn't fold; three or more is the common
 *  "silent investigation" shape that benefits from one collapsible
 *  block (#1057). */
const TOOL_GROUP_MIN_RUN = 3;

/** Synthetic toolName used for the folded group card. Namespaced with
 *  the `_aoe_` prefix so it can't collide with a real ACP tool kind. */
export const TOOL_GROUP_NAME = "_aoe_tool_group";

/** Synthetic toolName for a run of consecutive TodoWrite snapshots
 *  folded into one card. Distinct from TOOL_GROUP_NAME so the renderer
 *  dispatches it to TodoGroupCard (latest list always visible, history
 *  on expand) rather than the generic actions group. See #1468. */
export const TODO_GROUP_NAME = "_aoe_todo_group";

type GroupChildPayload = {
  toolCallId: string;
  toolName: string;
  argsText: string;
  // `endedAt` rides along from DraftPart.result (set in completeToolCall)
  // and StructuredView's pickEndedAt reads it to compute durations; keep it
  // on the type so a future rebuild of `result` doesn't drop it. `stopped`
  // rides the same way so grouped/subagent children reconstruct as the
  // distinct "stopped" terminal state instead of "done" (#1646).
  result?: { content: string; endedAt?: string; stopped?: boolean };
  isError?: boolean;
};

/** Flatten a run of tool-call parts into the verbatim child payload the
 *  group renderers reconstruct from. Shared by the generic actions
 *  group and the TodoWrite group (#1468). */
function buildGroupChildren(run: DraftPart[]): {
  childIds: string[];
  children: GroupChildPayload[];
} {
  const childIds: string[] = [];
  const children: GroupChildPayload[] = [];
  for (const p of run) {
    if (p.type !== "tool-call") continue;
    childIds.push(p.toolCallId);
    children.push({
      toolCallId: p.toolCallId,
      toolName: p.toolName,
      argsText: p.argsText,
      result: p.result,
      isError: p.isError,
    });
  }
  return { childIds, children };
}

/** Walk an assistant message's parts and collapse runs of consecutive
 *  tool-call parts (regardless of kind) into one synthetic group part
 *  when the run is ≥ TOOL_GROUP_MIN_RUN long. The grouping boundary is
 *  ANY non-tool-call part (text, callout, etc.); matching the "what
 *  did the agent do silently before its next sentence?" UX shape. The
 *  underlying tool-call data is preserved verbatim inside the group's
 *  argsText payload so the renderer can expand back to the original
 *  per-tool cards on click. */
function collapseToolRuns(parts: DraftPart[], todosEnabled: boolean): DraftPart[] {
  const out: DraftPart[] = [];
  let run: DraftPart[] = [];
  const flushRun = () => {
    if (run.length === 0) return;
    if (run.length < TOOL_GROUP_MIN_RUN) {
      for (const p of run) out.push(p);
    } else {
      // A run made up entirely of consecutive TodoWrite snapshots is
      // the spam case (#1468): fold it into one TodoGroupCard that
      // shows the latest list collapsed and the per-call history on
      // expand. TodoWrites are detected via the `_aoe_title` echo /
      // `todos` payload stashed in argsText.
      const isTodo = (p: DraftPart) => p.type === "tool-call" && isTodoWriteArgsText(p.argsText, todosEnabled);
      if (run.every(isTodo)) {
        const { childIds, children } = buildGroupChildren(run);
        out.push({
          type: "tool-call",
          // Anchor the group's React identity to the first child, not the
          // join of every child id. While the tail run is still growing,
          // appending a child would otherwise mutate the key, remount the
          // card, and reset its local expand state to collapsed. The first
          // child id is unique per run (runs are bounded by non-tool-call
          // parts) and stable across replay, so the card survives appends
          // and a user's expand sticks. See #2802.
          toolCallId: `todogroup-${childIds[0]}`,
          toolName: TODO_GROUP_NAME,
          argsText: JSON.stringify({ children }),
        });
        run = [];
        return;
      }
      // A run that MIXES TodoWrite with real tool work stays inline: a
      // status update sandwiched between Reads/Edits shouldn't be
      // hidden inside a generic actions group, and pulling it out
      // would reorder the timeline. See #1064, #1468.
      if (run.some(isTodo)) {
        for (const p of run) out.push(p);
        run = [];
        return;
      }
      // Subagent cards are already their own collapsible block (one card
      // per Task). Folding N parallel Tasks into a single generic group
      // card hides the parallelism the user dispatched. See #1041.
      const hasSubagent = run.some((p) => p.type === "tool-call" && p.toolName === SUBAGENT_TASK_NAME);
      if (hasSubagent) {
        for (const p of run) out.push(p);
        run = [];
        return;
      }
      const { childIds, children } = buildGroupChildren(run);
      out.push({
        type: "tool-call",
        // Anchor the group's React identity to the first child (see the
        // TodoGroup note above and #2802): the full-join key changed on
        // every append, remounting the card and re-collapsing it.
        toolCallId: `group-${childIds[0]}`,
        toolName: TOOL_GROUP_NAME,
        argsText: JSON.stringify({ children }),
      });
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
