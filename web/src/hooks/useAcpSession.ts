// Structured view session: reduces the ACP stream into AcpState and exposes the user's actions.
// Failed actions surface through `state.lastError` rather than being silently lost.

import { useCallback, useEffect, useMemo, useReducer, useRef, type Dispatch } from "react";
import {
  emptyAcpState,
  type AcpAttachment,
  type AcpState,
  type ApprovalDecision,
  type ElicitationResolution,
  type PromptAttachmentInput,
  type QueuedPrompt,
} from "../lib/acpTypes";
import {
  clearServerQueue,
  editServerQueuedPrompt,
  enqueueServerPrompt,
  listServerQueue,
  removeServerQueuedPrompt,
  reportAcpInteraction,
  setSessionArchive,
  setSessionSnooze,
} from "../lib/api";
import { classifyResolveResponse, reducer, type Action } from "./acpSession/reducer";
import { ACP_MAX_RETRIES, useAcpConnection } from "./acpSession/useAcpConnection";
import { cacheGet, cacheSet, sweepExpiredStorage } from "./acpSession/stateCache";
import { useLatestRef } from "./useLatestRef";

export { reducer, transcriptDeltaAction, type Action } from "./acpSession/reducer";
export { clearAcpCache, useBackgroundAgents } from "./acpSession/stateCache";

type PromptSendResult =
  | { kind: "dispatched" }
  | { kind: "queued"; queuedId: string }
  /** 503 `worker_not_ready`: the daemon resumed, the worker did not arrive, and nothing was
   *  published. The caller owns the text and must re-queue it. */
  | { kind: "worker_not_ready" }
  | { kind: "retryable_failure" }
  | { kind: "non_retryable_failure" };

/** `/acp/prompt` 202 body (Rust `PromptDispatchResponse`). */
interface PromptDispatchBody {
  disposition?: "sent" | "steered" | "queued";
  queued_id?: string;
}

function initialState(sessionId: string | null): AcpState {
  return (sessionId ? cacheGet(sessionId) : undefined) ?? emptyAcpState();
}

function acpUrl(sessionId: string, path: string): string {
  return `/api/sessions/${encodeURIComponent(sessionId)}/acp/${path}`;
}

function jsonPost(body: unknown): RequestInit {
  return { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) };
}

async function safeText(res: Response): Promise<string> {
  try {
    return (await res.text()).slice(0, 200);
  } catch {
    return "";
  }
}

function describeError(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** POST and report a failure as `Could not <verb>` or `Network error <gerund>`, calling `onFail` first. */
async function postReportingErrors(
  dispatch: Dispatch<Action>,
  url: string,
  init: RequestInit,
  [verb, gerund]: [string, string],
  onFail?: () => void,
): Promise<void> {
  try {
    const res = await fetch(url, init);
    if (res.ok) return;
    const detail = await safeText(res);
    onFail?.();
    dispatch({ kind: "error", message: `Could not ${verb} (${res.status}). ${detail}`.trim() });
  } catch (e) {
    onFail?.();
    dispatch({ kind: "error", message: `Network error ${gerund}: ${describeError(e)}` });
  }
}

// A UUID v4 even on plain-HTTP LAN hosts, where `crypto.randomUUID` is unavailable.
function optimisticPromptId(): string {
  const c = globalThis.crypto;
  if (c && typeof c.randomUUID === "function") return c.randomUUID();
  if (c && typeof c.getRandomValues === "function") {
    return "10000000-1000-4000-8000-100000000000".replace(/[018]/g, (digit) =>
      (Number(digit) ^ (c.getRandomValues(new Uint8Array(1))[0]! & (15 >> (Number(digit) / 4)))).toString(16),
    );
  }
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
}

export function useAcpSession(
  sessionId: string | null,
  /** Queued prompts can be sent immediately only while the worker is running. */
  workerState: "absent" | "resuming" | "running" | "stopping" = "running",
  archivedAt: string | null = null,
  snoozedUntil: string | null = null,
) {
  sweepExpiredStorage();
  const [state, dispatch] = useReducer(reducer, sessionId, initialState);
  const archivedAtRef = useLatestRef(archivedAt);
  const snoozedUntilRef = useLatestRef(snoozedUntil);
  const sessionIdRef = useLatestRef(sessionId);
  useEffect(() => {
    if (sessionIdRef.current) cacheSet(sessionIdRef.current, state);
  }, [state, sessionIdRef]);
  const queuedPromptsRef = useLatestRef(state.queuedPrompts);
  const connection = useAcpConnection(sessionId, sessionIdRef, state, dispatch);
  const { status, lastActivityRef } = connection;

  const resolveApproval = useCallback(
    async (nonce: string, decision: ApprovalDecision, optionId?: string) => {
      if (!sessionId) return;
      try {
        const res = await fetch(
          acpUrl(sessionId, `approvals/${encodeURIComponent(nonce)}`),
          jsonPost(optionId === undefined ? { decision } : { decision, option_id: optionId }),
        );
        const detail = res.ok ? "" : await safeText(res);
        const outcome = classifyResolveResponse("approval", res.ok, res.status, detail, nonce);
        dispatch(
          outcome.kind === "resolved"
            ? { kind: "approval_resolved_locally", nonce }
            : { kind: "error", message: outcome.message },
        );
      } catch (e) {
        dispatch({ kind: "error", message: `Network error resolving approval: ${describeError(e)}` });
      }
    },
    [sessionId],
  );

  // Throws on failure so the card re-enables and the same nonce can be resubmitted.
  const resolveElicitation = useCallback(
    async (nonce: string, resolution: ElicitationResolution) => {
      if (!sessionId) return;
      let res: Response;
      try {
        res = await fetch(acpUrl(sessionId, `elicitations/${encodeURIComponent(nonce)}`), jsonPost(resolution));
      } catch (e) {
        dispatch({ kind: "error", message: `Network error resolving question: ${describeError(e)}` });
        throw e;
      }
      const detail = res.ok ? "" : await safeText(res);
      const outcome = classifyResolveResponse("elicitation", res.ok, res.status, detail, nonce);
      if (outcome.kind === "resolved") {
        dispatch({ kind: "elicitation_resolved_locally", nonce, resolution });
        return;
      }
      dispatch({ kind: "error", message: outcome.message });
      throw new Error(outcome.message);
    },
    [sessionId],
  );

  // POST a prompt behind an optimistic overlay row keyed by its prompt_id and report the outcome.
  const dispatchPromptNow = useCallback(
    async (text: string, attachments?: PromptAttachmentInput[]): Promise<PromptSendResult> => {
      if (!sessionId) return { kind: "retryable_failure" };
      const previews: AcpAttachment[] = (attachments ?? []).map((a, i) => ({
        id: `local-${Date.now()}-${i}`,
        kind: a.kind,
        mimeType: a.mimeType,
        name: a.name,
        size: Math.floor((a.dataB64.length * 3) / 4),
        url: `data:${a.mimeType};base64,${a.dataB64}`,
      }));
      const promptId = optimisticPromptId();
      dispatch({ kind: "user_prompt", id: promptId, text, attachments: previews.length > 0 ? previews : undefined });
      lastActivityRef.current = Date.now();
      try {
        const res = await fetch(
          acpUrl(sessionId, "prompt"),
          jsonPost({
            text,
            prompt_id: promptId,
            attachments: (attachments ?? []).map((a) => ({
              kind: a.kind,
              mime_type: a.mimeType,
              data: a.dataB64,
              name: a.name,
            })),
          }),
        );
        if (!res.ok) {
          const detail = await safeText(res);
          const rejected = res.status >= 400 && res.status < 500;
          // The worker is not up yet (idle-stopped or parked): the caller re-queues it, so no banner.
          const workerNotReady = res.status === 503 && detail.startsWith("worker_not_ready");
          const settle = rejected
            ? "prompt_send_rejected"
            : workerNotReady
              ? "rollback_optimistic_prompt"
              : "settle_inflight_prompt";
          dispatch({ kind: settle, id: promptId });
          if (!workerNotReady) {
            dispatch({ kind: "error", message: `Could not send prompt (${res.status}). ${detail}`.trim() });
          }
          if (workerNotReady) return { kind: "worker_not_ready" };
          return { kind: rejected ? "non_retryable_failure" : "retryable_failure" };
        }
        let body: PromptDispatchBody = {};
        try {
          body = ((await res.json()) as PromptDispatchBody | null) ?? {};
        } catch {
          // No body means it was dispatched.
        }
        if (body.disposition === "queued") {
          // Parked by the daemon: the prompt becomes a queue row, not a transcript row.
          dispatch({ kind: "rollback_optimistic_prompt", id: promptId });
          return { kind: "queued", queuedId: body.queued_id ?? promptId };
        }
        return { kind: "dispatched" };
      } catch (e) {
        // Settling is the safe direction: a false idle converges on the next frame, a false active never does.
        dispatch({ kind: "settle_inflight_prompt", id: promptId });
        dispatch({ kind: "error", message: `Network error sending prompt: ${describeError(e)}` });
        return { kind: "retryable_failure" };
      }
    },
    [sessionId, lastActivityRef],
  );

  const enqueueServer = useCallback(
    (text: string, attachments?: PromptAttachmentInput[]) => {
      if (!sessionId) return;
      const id = `q-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
      dispatch({ kind: "enqueue_prompt", id, text, attachments });
      reportAcpInteraction("prompt_queued");
      void (async () => {
        const row = await enqueueServerPrompt(sessionId, { id, text, attachments });
        dispatch(
          row
            ? { kind: "confirm_queued_prompt", id }
            : {
                kind: "error",
                message: "Couldn't queue your message on the server; it may not send. Remove it and try again.",
              },
        );
      })();
    },
    [sessionId],
  );

  const showServerQueued = useCallback((id: string, text: string, attachments?: PromptAttachmentInput[]) => {
    dispatch({ kind: "enqueue_prompt", id, text, attachments });
    dispatch({ kind: "confirm_queued_prompt", id });
  }, []);

  const sendPrompt = useCallback(
    async (text: string, attachments?: PromptAttachmentInput[]) => {
      if (!sessionId) return;
      // The reconciler skips archived and snoozed sessions, so wake them before sending.
      if (archivedAtRef.current || snoozedUntilRef.current) {
        const woke = archivedAtRef.current
          ? await setSessionArchive(sessionId, false)
          : await setSessionSnooze(sessionId, null);
        if (!woke) {
          dispatch({
            kind: "error",
            message: "Could not wake this session. Please retry, or unarchive / unsnooze from the sidebar.",
          });
          return;
        }
      }
      const result = await dispatchPromptNow(text, attachments);
      if (result.kind === "queued") {
        showServerQueued(result.queuedId, text, attachments);
        reportAcpInteraction("prompt_queued");
        return;
      }
      // The daemon sent, resumed, and no worker arrived, so nothing was published. Keyed on its
      // answer, not on this client's view of which states are wakeable, which can lag the daemon.
      if (result.kind === "worker_not_ready") {
        enqueueServer(text, attachments);
      }
    },
    [sessionId, archivedAtRef, snoozedUntilRef, dispatchPromptNow, enqueueServer, showServerQueued],
  );

  // The daemon owns and drains the queue; re-list on connect and at each turn change. The first
  // list per session migrates rows queued locally before the server owned the queue.
  const queueMigratedRef = useRef<Set<string>>(new Set());
  useEffect(() => {
    if (!sessionId || status !== "open") return;
    let cancelled = false;
    void (async () => {
      if (!queueMigratedRef.current.has(sessionId)) {
        queueMigratedRef.current.add(sessionId);
        for (const q of queuedPromptsRef.current) {
          if (cancelled) return;
          await enqueueServerPrompt(sessionId, {
            id: q.id,
            text: q.text,
            createdAt: q.queuedAt,
            attachments: q.attachments,
          });
        }
      }
      const rows = await listServerQueue(sessionId);
      if (!cancelled) dispatch({ kind: "hydrate_server_queue", rows });
    })();
    return () => {
      cancelled = true;
    };
  }, [sessionId, status, state.turnActive, queuedPromptsRef]);

  // Queue mutations are optimistic; a failed server call is corrected by the next hydrate.
  const removeQueuedPrompt = useCallback(
    (id: string) => {
      dispatch({ kind: "dequeue_prompt", id });
      if (sessionId) void removeServerQueuedPrompt(sessionId, id);
    },
    [sessionId],
  );

  const editQueuedPrompt = useCallback(
    (id: string, text: string) => {
      dispatch({ kind: "edit_queued_prompt", id, text });
      if (sessionId) void editServerQueuedPrompt(sessionId, id, text);
    },
    [sessionId],
  );

  const clearQueue = useCallback(() => {
    dispatch({ kind: "clear_queue" });
    if (sessionId) void clearServerQueue(sessionId);
  }, [sessionId]);

  // Never bind this to Escape: an accidental press would abort work the user meant to keep.
  const cancelPrompt = useCallback(async () => {
    if (!sessionId) return;
    await postReportingErrors(dispatch, acpUrl(sessionId, "cancel"), { method: "POST" }, ["cancel", "cancelling"]);
  }, [sessionId]);

  const steerable = !!state.promptCapabilities?.steering && !state.cancelling && !state.compacting;

  // A non-steerable running turn is cancelled and the server drains the row instead.
  const sendQueuedNow = useCallback(
    async (prompt: QueuedPrompt) => {
      const sid = sessionIdRef.current;
      if (!sid) return;
      if (state.turnActive && !steerable) {
        await cancelPrompt();
        return;
      }
      // Removing a row whose bytes live only on the server would delete them before the send.
      if (prompt.attachments?.some((a) => !a.dataB64)) return;
      // Remove server-side first so the drain can't also deliver it; a failed remove means it already did.
      dispatch({ kind: "dequeue_prompt", id: prompt.id });
      if (!(await removeServerQueuedPrompt(sid, prompt.id))) return;
      const result = await dispatchPromptNow(prompt.text, prompt.attachments);
      if (result.kind === "queued") {
        showServerQueued(result.queuedId, prompt.text, prompt.attachments);
      } else if (result.kind === "retryable_failure" || result.kind === "worker_not_ready") {
        enqueueServer(prompt.text, prompt.attachments);
      }
    },
    [sessionIdRef, state.turnActive, steerable, cancelPrompt, dispatchPromptNow, enqueueServer, showServerQueued],
  );

  // Pessimistic: the value changes when the adapter confirms; pending only dims the clicked option.
  const setConfigOption = useCallback(
    async (configId: string, value: string) => {
      if (!sessionId) return;
      dispatch({ kind: "set_pending_config_option", configId, value });
      await postReportingErrors(
        dispatch,
        acpUrl(sessionId, "config-option"),
        jsonPost({ config_id: configId, value }),
        [`set ${configId}`, `setting ${configId}`],
        () => dispatch({ kind: "clear_pending_config_option_if_match", configId, value }),
      );
    },
    [sessionId],
  );

  // The server publishes the resulting Stopped event; no client-side Stopped is fabricated.
  const forceEndTurn = useCallback(async () => {
    if (!sessionId) return;
    lastActivityRef.current = Date.now();
    await postReportingErrors(dispatch, acpUrl(sessionId, "force_end_turn"), { method: "POST" }, [
      "force end turn",
      "forcing end turn",
    ]);
  }, [sessionId, lastActivityRef]);

  const dismissers = useMemo(
    () => ({
      dismissError: () => dispatch({ kind: "clear_error" }),
      dismissPrimer: () => dispatch({ kind: "dismiss_primer" }),
      dismissCompactionReminder: () => dispatch({ kind: "dismiss_compaction_reminder" }),
      dismissRejectedPrompt: (id: string) => dispatch({ kind: "dismiss_rejected_prompt", id }),
      dismissModeSwitchFailed: () => dispatch({ kind: "dismiss_mode_switch_failed" }),
      dismissConfigOptionSwitchFailed: () => dispatch({ kind: "dismiss_config_option_switch_failed" }),
    }),
    [],
  );

  return {
    state,
    ...connection,
    maxRetries: ACP_MAX_RETRIES,
    resolveApproval,
    resolveElicitation,
    sendPrompt,
    cancelPrompt,
    forceEndTurn,
    ...dismissers,
    removeQueuedPrompt,
    editQueuedPrompt,
    clearQueue,
    sendQueuedNow,
    canSendQueuedNow:
      status === "open" &&
      !state.workerStopped &&
      !state.workerRestarting &&
      // Same mirror: "Send now" is offered wherever a prompt can start a worker, which is what
      // lets a row queued before the park drain without waiting out the reset window.
      (workerState === "running" ||
        state.workerIdleStopped ||
        state.rateLimitRetriesExhausted ||
        state.rateLimitParked),
    /** Send now would cancel a running, non-steerable turn rather than send immediately. */
    sendNowInterruptsTurn: state.turnActive && !steerable,
    setConfigOption,
  };
}
