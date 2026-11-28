// Structured view session: reduces the ACP stream into AcpState and exposes the user's actions.
// Failed actions surface through transient interaction errors without replacing operation diagnostics.

import { useCallback, useEffect, useLayoutEffect, useMemo, useReducer, useRef, type Dispatch } from "react";
import {
  emptyAcpState,
  nativeInputAvailable,
  canDecideRequest,
  canSubmitPromptIntent,
  SANDBOX_LOGIN_REQUIRED_CODE,
  type AcpAttachment,
  type AcpState,
  type ApprovalDecision,
  type ElicitationResolution,
  type PromptAttachmentInput,
  type QueuedPrompt,
  type RejectedPrompt,
  type StartupCause,
} from "../lib/acpTypes";
import {
  cancelSubagent,
  clearServerQueue,
  editServerQueuedPrompt,
  enqueueServerPrompt,
  listServerQueue,
  loadStoredAttachment,
  removeServerQueuedPrompt,
  reportAcpInteraction,
  setSessionSnooze,
} from "../lib/api";
import { classifyResolveResponse, reducer, type Action } from "./acpSession/reducer";
import { ACP_MAX_RETRIES, useAcpConnection } from "./acpSession/useAcpConnection";
import {
  cacheGet,
  cacheSet,
  flushPersist,
  sweepExpiredStorage,
  registerLiveAcpActions,
  ownsLiveAcpActions,
  NATIVE_INPUT_UNCONFIRMED,
} from "./acpSession/stateCache";
import { setLiveQueueCount } from "../lib/acpStateStorage";
import { useLatestRef } from "./useLatestRef";

export { reducer, transcriptDeltaAction, type Action } from "./acpSession/reducer";
export {
  clearAcpCache,
  useActivity,
  useBackgroundAgents,
  useBackgroundShells,
  useBackgroundTasks,
  useSubagents,
  useWorkflowRuns,
} from "./acpSession/stateCache";

type PromptSendResult =
  | { kind: "dispatched" }
  | { kind: "queued"; queuedId: string }
  /** 503 `worker_not_ready`: the daemon resumed, the worker did not arrive, and nothing was
   *  published. The caller owns the text and must re-queue it. */
  | { kind: "worker_not_ready" }
  /** 409 `queued_prompt_gone`: the drain or another tab already delivered the named queue row. */
  | { kind: "queued_gone" }
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

function jsonPost(body: unknown, headers: Record<string, string> = {}): RequestInit {
  return { method: "POST", headers: { "Content-Type": "application/json", ...headers }, body: JSON.stringify(body) };
}

/** Self-identifies the dashboard to the daemon, which records it as the prompt's origin. */
const PROMPT_ORIGIN_HEADERS = { "X-Aoe-Prompt-Origin": "dashboard" };

async function safeText(res: Response, limit = 200): Promise<string> {
  try {
    return (await res.text()).slice(0, limit);
  } catch {
    return "";
  }
}

/** Shown when the daemon kept a prompt because the session's `before_start` hook failed: plain words, never the hook's output. */
const HOOK_FAILED_QUEUED_MESSAGE =
  "The session's before_start hook failed; your message is queued and will be sent when it works.";

type KeptPrompt = {
  queuedId: string;
  message: string;
  cause: StartupCause | null;
};

/** The queue id of a prompt the daemon kept until the session can launch (sandbox login or `before_start` hook), with what to tell the user. */
function keptPrompt(status: number, body: string): KeptPrompt | undefined {
  try {
    const parsed = JSON.parse(body) as {
      error?: unknown;
      queued_id?: unknown;
      message?: unknown;
      command?: unknown;
      profile?: unknown;
    };
    if (typeof parsed.queued_id !== "string") return undefined;
    if (status === 409 && parsed.error === SANDBOX_LOGIN_REQUIRED_CODE) {
      return {
        queuedId: parsed.queued_id,
        message: typeof parsed.message === "string" ? parsed.message : "",
        cause: {
          kind: "sandbox_login_required",
          // Present only when the recovery needs `--command`; the login the banner starts takes it.
          ...(typeof parsed.command === "string" && { command: parsed.command }),
          // Present only for a session of another profile than the daemon's; the login runs in that profile's file.
          ...(typeof parsed.profile === "string" && { profile: parsed.profile }),
        },
      };
    }
    if (status === 502 && parsed.error === "before_start_hook_failed") {
      return {
        queuedId: parsed.queued_id,
        message: HOOK_FAILED_QUEUED_MESSAGE,
        cause: null,
      };
    }
  } catch {
    // Not JSON: a plain-text error.
  }
  return undefined;
}

function describeError(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** Return a POST's user-facing failure, calling `onFail` before the caller reports it. */
async function postErrorMessage(
  url: string,
  init: RequestInit,
  [verb, gerund]: [string, string],
  onFail?: () => void,
): Promise<string | null> {
  try {
    const res = await fetch(url, init);
    if (res.ok) return null;
    const detail = await safeText(res);
    onFail?.();
    return `Could not ${verb} (${res.status}). ${detail}`.trim();
  } catch (e) {
    onFail?.();
    return `Network error ${gerund}: ${describeError(e)}`;
  }
}

// POSTs of queued rows belong to the Session, not to a mounted hook. Re-sends of rows the server
// never confirmed and prompts queued while open run one after another, and each skips a row an
// earlier one posted. A removal or edit meanwhile finds no row on the server yet, so it is applied
// to the POST, or after it when the POST was already in flight.
interface QueuePosts {
  tail: Promise<void>;
  posted: Set<string>;
  removed: Set<string>;
  edited: Map<string, string>;
}
const queuePosts = new Map<string, QueuePosts>();
// The mounted hooks showing each Session; with none, an acknowledgment goes to its stored state.
const queueAckTargets = new Map<string, Map<Dispatch<Action>, () => boolean>>();
const queueMigrations = new Map<string, Promise<ReadonlySet<string>>>();
const attemptedMigrations = new Map<string, Set<string>>();
const queueEdits = new Map<string, Promise<unknown>>();

export function pendingRowIds(state: AcpState): ReadonlySet<string> {
  return new Set(state.queuedPrompts.filter((q) => q.pending).map((q) => q.id));
}

/** True when a row became pending (enqueued) or stopped being pending (confirmed, removed, delivered). */
export function pendingRowsChanged(before: ReadonlySet<string>, state: AcpState): boolean {
  const now = pendingRowIds(state);
  return now.size !== before.size || [...now].some((id) => !before.has(id));
}

function acknowledgeQueued(sessionId: string, action: Action): void {
  const targets = queueAckTargets.get(sessionId);
  if (targets?.size) {
    for (const target of targets.keys()) target(action);
    return;
  }
  const stored = cacheGet(sessionId);
  if (!stored) return;
  cacheSet(sessionId, reducer(stored, action));
  flushPersist(sessionId);
}

function postQueuedInOrder(sessionId: string, task: (entry: QueuePosts) => Promise<void>): Promise<void> {
  const entry = queuePosts.get(sessionId) ?? {
    tail: Promise.resolve(),
    posted: new Set<string>(),
    removed: new Set<string>(),
    edited: new Map<string, string>(),
  };
  const run = entry.tail.then(() => task(entry));
  entry.tail = run;
  queuePosts.set(sessionId, entry);
  const settle = () => {
    if (queuePosts.get(sessionId)?.tail === run) queuePosts.delete(sessionId);
  };
  run.then(settle, settle);
  return run;
}

/** Posts one row and confirms it; false only when its POST failed. */
async function postQueuedRow(
  sessionId: string,
  entry: QueuePosts,
  row: Parameters<typeof enqueueServerPrompt>[1],
): Promise<boolean> {
  if (entry.posted.has(row.id) || entry.removed.has(row.id)) return true;
  let sent = entry.edited.get(row.id) ?? row.text;
  const saved = await enqueueServerPrompt(sessionId, { ...row, text: sent });
  if (!saved) return false;
  // A DELETE or PATCH sent while this POST was in flight may have reached the server first.
  if (entry.removed.has(row.id)) {
    await removeServerQueuedPrompt(sessionId, row.id);
    return true;
  }
  let latest = entry.edited.get(row.id);
  while (latest !== undefined && latest !== sent) {
    sent = latest;
    await editServerQueuedPrompt(sessionId, row.id, sent);
    latest = entry.edited.get(row.id);
  }
  entry.posted.add(row.id);
  // Confirmed, so a list that no longer has it (delivered) drops it instead of keeping it to re-send.
  acknowledgeQueued(sessionId, { kind: "confirm_queued_prompt", id: row.id });
  return true;
}

async function migrateQueue(
  sessionId: string,
  rows: QueuedPrompt[],
  admitted: () => boolean,
): Promise<ReadonlySet<string>> {
  const confirmed = new Set<string>();
  const attempted = attemptedMigrations.get(sessionId) ?? new Set<string>();
  attemptedMigrations.set(sessionId, attempted);
  await postQueuedInOrder(sessionId, async (entry) => {
    for (const q of rows) {
      if (!admitted()) break;
      // A missing response does not prove the daemon did not deliver the row.
      if (attempted.has(q.id)) continue;
      attempted.add(q.id);
      await postQueuedRow(sessionId, entry, { id: q.id, text: q.text, attachments: q.attachments });
      if (entry.posted.has(q.id)) confirmed.add(q.id);
    }
  });
  return confirmed;
}

function markQueuedRemoved(sessionId: string, ids: Iterable<string>): void {
  const entry = queuePosts.get(sessionId);
  if (entry) for (const id of ids) entry.removed.add(id);
}

function markQueuedEdited(sessionId: string, id: string, text: string): void {
  queuePosts.get(sessionId)?.edited.set(id, text);
}

/** Waits before re-listing after a failed queue list; then the next poll change or turn retries. */
const QUEUE_LIST_RETRY_MS = [2000, 5000, 15000];

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
  /** Explicit worker shutdown also disables queue delivery while it is in progress. */
  workerState: "absent" | "resuming" | "running" | "stopping" = "running",
  archivedAt: string | null = null,
  snoozedUntil: string | null = null,
  /** The session list's `queued_count`; a change means another client may have changed the queue. */
  polledQueuedCount?: number,
) {
  sweepExpiredStorage();
  const [state, dispatch] = useReducer(reducer, sessionId, initialState);
  const archivedAtRef = useLatestRef(archivedAt);
  const snoozedUntilRef = useLatestRef(snoozedUntil);
  const sessionIdRef = useLatestRef(sessionId);
  const unconfirmedRef = useRef<ReadonlySet<string>>(new Set());
  useEffect(() => {
    if (!sessionIdRef.current) return;
    cacheSet(sessionIdRef.current, state);
    // An unconfirmed row is the only retry copy, and a confirmed one must not be re-sent by another window, so persist both now.
    if (pendingRowsChanged(unconfirmedRef.current, state)) flushPersist(sessionIdRef.current);
    unconfirmedRef.current = pendingRowIds(state);
  }, [state, sessionIdRef]);
  // Separate from the `[state]` effect: its cleanup runs on every state change and would defeat the throttle.
  useEffect(
    () => () => {
      // Cleanup on session switch or unmount.
      // eslint-disable-next-line react-you-might-not-need-an-effect/no-event-handler
      if (sessionId) flushPersist(sessionId);
    },
    [sessionId],
  );
  const queuedPromptsRef = useLatestRef(state.queuedPrompts);
  const connection = useAcpConnection(sessionId, sessionIdRef, state, dispatch);
  const { status, lastActivityRef } = connection;
  const inputStateRef = useRef(state);
  const connectionStatusRef = useRef(status);
  useLayoutEffect(() => {
    inputStateRef.current = state;
    connectionStatusRef.current = status;
  }, [state, status]);
  const actionOwner = useMemo(() => Symbol(sessionId ?? undefined), [sessionId]);
  const requireNativeInput = useCallback(
    (
      target?: NonNullable<AcpState["interactionError"]>["target"],
      mode: "native" | "decision" | "prompt_intent" | "authorized_snooze_wake" = "native",
    ): boolean => {
      const current = inputStateRef.current;
      const allowed =
        mode === "decision"
          ? !!target && !archivedAtRef.current && canDecideRequest(current, target.nonce)
          : mode === "native"
            ? nativeInputAvailable(current)
            : mode === "prompt_intent"
              ? canSubmitPromptIntent(current)
              : current.nativeSnapshotSeq != null && current.connectionUnavailable === null;
      if (connectionStatusRef.current === "open" && allowed) return true;
      dispatch({ kind: "error", message: NATIVE_INPUT_UNCONFIRMED, target });
      return false;
    },
    [inputStateRef, connectionStatusRef, archivedAtRef],
  );

  // Reject failures so the card rolls back and the same nonce remains retryable.
  const resolveApproval = useCallback(
    async (nonce: string, decision: ApprovalDecision, optionId?: string) => {
      if (!sessionId) return;
      if (!requireNativeInput({ kind: "approval", nonce }, "decision")) throw new Error(NATIVE_INPUT_UNCONFIRMED);
      let res: Response;
      try {
        res = await fetch(
          acpUrl(sessionId, `approvals/${encodeURIComponent(nonce)}`),
          jsonPost(optionId === undefined ? { decision } : { decision, option_id: optionId }),
        );
      } catch (e) {
        dispatch({
          kind: "error",
          message: `Network error resolving approval: ${describeError(e)}`,
          target: { kind: "approval", nonce },
        });
        throw e;
      }
      const detail = res.ok ? "" : await safeText(res);
      const outcome = classifyResolveResponse("approval", res.ok, res.status, detail, nonce);
      if (outcome.kind === "resolved") {
        dispatch({ kind: "approval_resolved_locally", nonce });
        return;
      }
      dispatch({ kind: "error", message: outcome.message, target: { kind: "approval", nonce } });
      throw new Error(outcome.message);
    },
    [sessionId, requireNativeInput],
  );

  const resolveElicitation = useCallback(
    async (nonce: string, resolution: ElicitationResolution) => {
      if (!sessionId) return;
      if (!requireNativeInput({ kind: "elicitation", nonce }, "decision")) throw new Error(NATIVE_INPUT_UNCONFIRMED);
      let res: Response;
      try {
        res = await fetch(acpUrl(sessionId, `elicitations/${encodeURIComponent(nonce)}`), jsonPost(resolution));
      } catch (e) {
        dispatch({
          kind: "error",
          message: `Network error resolving question: ${describeError(e)}`,
          target: { kind: "elicitation", nonce },
        });
        throw e;
      }
      const detail = res.ok ? "" : await safeText(res);
      const outcome = classifyResolveResponse("elicitation", res.ok, res.status, detail, nonce);
      if (outcome.kind === "resolved") {
        dispatch({ kind: "elicitation_resolved_locally", nonce, resolution });
        return;
      }
      dispatch({ kind: "error", message: outcome.message, target: { kind: "elicitation", nonce } });
      throw new Error(outcome.message);
    },
    [sessionId, requireNativeInput],
  );

  // POST a prompt behind an optimistic overlay row keyed by its prompt_id and report the outcome.
  const dispatchPromptNow = useCallback(
    async (
      text: string,
      attachments?: PromptAttachmentInput[],
      queuedPromptId?: string,
      admission: "native" | "prompt_intent" | "authorized_snooze_wake" = "prompt_intent",
    ): Promise<PromptSendResult> => {
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
      if (!requireNativeInput(undefined, admission)) {
        dispatch({ kind: "prompt_send_rejected", id: promptId });
        dispatch({ kind: "prompt_answered", promptId });
        return { kind: "non_retryable_failure" };
      }
      lastActivityRef.current = Date.now();
      try {
        const res = await fetch(
          acpUrl(sessionId, "prompt"),
          jsonPost(
            {
              text,
              prompt_id: promptId,
              ...(admission === "native" ? { no_revive: true } : {}),
              ...(queuedPromptId ? { queued_prompt_id: queuedPromptId } : {}),
              attachments: (attachments ?? []).map((a) => ({
                kind: a.kind,
                mime_type: a.mimeType,
                data: a.dataB64,
                name: a.name,
              })),
            },
            PROMPT_ORIGIN_HEADERS,
          ),
        );
        if (!res.ok) {
          const body = await safeText(res, Infinity);
          const detail = body.slice(0, 200);
          if (res.status === 409 && detail.startsWith("queued_prompt_gone")) {
            dispatch({ kind: "rollback_optimistic_prompt", id: promptId });
            return { kind: "queued_gone" };
          }
          const kept = keptPrompt(res.status, body);
          if (kept) {
            dispatch({ kind: "rollback_optimistic_prompt", id: promptId });
            // The queue cannot advance until the login: say what to run.
            if (kept.message) {
              dispatch({
                kind: "blocked_launch",
                message: kept.message,
                cause: kept.cause,
                promptId,
              });
            }
            return { kind: "queued", queuedId: kept.queuedId };
          }
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
      } finally {
        dispatch({ kind: "prompt_answered", promptId });
      }
    },
    [sessionId, lastActivityRef, requireNativeInput],
  );

  const enqueueServer = useCallback(
    (text: string, attachments?: PromptAttachmentInput[]) => {
      if (!sessionId) return;
      const id = `q-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
      dispatch({ kind: "enqueue_prompt", id, text, attachments });
      reportAcpInteraction("prompt_queued");
      void postQueuedInOrder(sessionId, async (entry) => {
        if (await postQueuedRow(sessionId, entry, { id, text, attachments })) return;
        dispatch({
          kind: "error",
          message: "Couldn't queue your message on the server; it may not send. Remove it and try again.",
        });
      });
    },
    [sessionId],
  );

  const showServerQueued = useCallback((id: string, text: string, attachments?: PromptAttachmentInput[]) => {
    dispatch({ kind: "enqueue_prompt", id, text, attachments, preserveExisting: true });
    dispatch({ kind: "confirm_queued_prompt", id });
  }, []);

  const sendPrompt = useCallback(
    async (text: string, attachments?: PromptAttachmentInput[]) => {
      if (!sessionId) return;
      // An archived session never starts on a prompt; the daemon refuses it too.
      if (archivedAtRef.current) {
        dispatch({ kind: "error", message: "session is archived; unarchive it first" });
        return;
      }
      // Refuse before waking or touching the queue. Opening a socket, replay,
      // and cached lifecycle flags are not current intent admission.
      if (connectionStatusRef.current !== "open" || !canSubmitPromptIntent(inputStateRef.current)) {
        await dispatchPromptNow(text, attachments);
        return;
      }
      const authorizedSnoozeWake = !!snoozedUntilRef.current && inputStateRef.current.promptWakeEligible;
      // The reconciler skips snoozed sessions, so wake them before sending.
      if (snoozedUntilRef.current && !(await setSessionSnooze(sessionId, null))) {
        dispatch({
          kind: "error",
          message: "Could not wake this session. Please retry, or unsnooze from the sidebar.",
        });
        return;
      }
      // Our own authorized unsnooze can consume its eligibility before the POST.
      // Carry only that single existing wake intent, never generic Unknown;
      // loss still refuses here and actual delivery rechecks the core fences.
      const result = await dispatchPromptNow(
        text,
        attachments,
        undefined,
        authorizedSnoozeWake ? "authorized_snooze_wake" : "prompt_intent",
      );
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
    [
      sessionId,
      archivedAtRef,
      snoozedUntilRef,
      dispatchPromptNow,
      enqueueServer,
      showServerQueued,
      inputStateRef,
      connectionStatusRef,
    ],
  );

  const retryOperationInFlightRef = useRef(false);
  const retryOperation = useCallback(
    async (text: string, attachments: PromptAttachmentInput[] | undefined, expected: string): Promise<boolean> => {
      if (retryOperationInFlightRef.current) return false;
      const current = inputStateRef.current;
      if (
        connectionStatusRef.current !== "open" ||
        !nativeInputAvailable(current) ||
        current.recovery?.expected !== expected ||
        !current.recovery.actions.retry_operation
      ) {
        dispatch({ kind: "error", message: "Operation retry is no longer offered; no input sent." });
        return false;
      }
      retryOperationInFlightRef.current = true;
      try {
        const result = await dispatchPromptNow(text, attachments, undefined, "native");
        if (result.kind === "queued") {
          showServerQueued(result.queuedId, text, attachments);
          return true;
        }
        if (result.kind === "worker_not_ready") {
          dispatch({ kind: "error", message: "The existing Agent is unavailable; input was not queued or replayed." });
        }
        return result.kind === "dispatched";
      } finally {
        retryOperationInFlightRef.current = false;
      }
    },
    [inputStateRef, connectionStatusRef, dispatchPromptNow, showServerQueued],
  );

  const queueMigrationEligible = status === "open" && canSubmitPromptIntent(state);
  /** Resends a refused prompt as it was sent, attachments included. */
  const retryRejectedPrompt = useCallback(
    async (prompt: RejectedPrompt) => {
      const loaded = await Promise.all((prompt.attachments ?? []).map(loadStoredAttachment));
      const attachments = loaded.filter((a): a is PromptAttachmentInput => a !== null);
      // A partial resend would differ from what the user sent.
      if (attachments.length < loaded.length) {
        dispatch({
          kind: "error",
          message:
            "Could not reload this prompt's attachments, so it was not resent. Copy its text to send it without them.",
        });
        return;
      }
      await sendPrompt(prompt.text, attachments.length > 0 ? attachments : undefined);
    },
    [sendPrompt],
  );

  // The daemon owns and drains the queue; re-list on connect, at each turn change, and when the
  // polled count changes (not whenever it differs: optimistic rows here differ until the next
  // poll). The first list per session waits for rows the server never confirmed to be re-sent,
  // once each; a rerun must not cancel that. A confirmed row missing from the server was
  // drained and must not be re-created.
  useEffect(() => {
    if (!sessionId) return;
    const targets = queueAckTargets.get(sessionId) ?? new Map<Dispatch<Action>, () => boolean>();
    targets.set(dispatch, () => connectionStatusRef.current === "open" && canSubmitPromptIntent(inputStateRef.current));
    queueAckTargets.set(sessionId, targets);
    return () => {
      targets.delete(dispatch);
      if (!targets.size && queueAckTargets.get(sessionId) === targets) queueAckTargets.delete(sessionId);
    };
  }, [sessionId]);
  // The sidebar badge takes this browser's count only between the first queue list after an open and the close.
  const queueListedRef = useRef(false);
  useEffect(() => {
    if (!sessionId || status !== "open") return;
    return () => {
      queueListedRef.current = false;
      setLiveQueueCount(sessionId, null);
    };
  }, [sessionId, status]);
  useEffect(() => {
    // eslint-disable-next-line react-you-might-not-need-an-effect/no-event-handler -- syncs reducer state to the sidebar's store
    if (!sessionId || status !== "open" || !queueListedRef.current) return;
    setLiveQueueCount(sessionId, state.queuedPrompts.length);
  }, [sessionId, status, state.queuedPrompts]);
  useEffect(() => {
    if (!sessionId || status !== "open") return;
    let cancelled = false;
    let migration = queueMigrations.get(sessionId);
    if (!migration && queueMigrationEligible) {
      migration = migrateQueue(
        sessionId,
        queuedPromptsRef.current.filter((q) => q.pending),
        () => Array.from(queueAckTargets.get(sessionId)?.values() ?? []).some((admitted) => admitted()),
      );
      queueMigrations.set(sessionId, migration);
      const pending = migration;
      const settle = () => {
        if (queueMigrations.get(sessionId) === pending) queueMigrations.delete(sessionId);
      };
      pending.then(settle, settle);
    }
    const migrated = migration;
    let retry: ReturnType<typeof setTimeout> | undefined;
    void (async () => {
      const migrationConfirmed = queueMigrationEligible ? await migrated : undefined;
      for (const delay of [...QUEUE_LIST_RETRY_MS, null]) {
        await queueEdits.get(sessionId);
        if (cancelled) return;
        const queueAtRead = queuedPromptsRef.current;
        const rows = await listServerQueue(sessionId);
        if (cancelled) return;
        if (rows !== null && queueAtRead !== queuedPromptsRef.current) continue;
        if (rows !== null) {
          queueListedRef.current = true;
          dispatch({ kind: "hydrate_server_queue", rows, queueAtRead, migrationConfirmed });
          return;
        }
        // A failed list must not replace the rows on screen. The sidebar falls back to the polled
        // count while this retries, then waits for the next trigger.
        queueListedRef.current = false;
        setLiveQueueCount(sessionId, null);
        if (delay === null) return;
        await new Promise((resolve) => (retry = setTimeout(resolve, delay)));
      }
    })();
    return () => {
      cancelled = true;
      clearTimeout(retry);
    };
  }, [
    sessionId,
    status,
    state.turnActive,
    queuedPromptsRef,
    polledQueuedCount,
    queueMigrationEligible,
    inputStateRef,
    connectionStatusRef,
  ]);

  // Queue mutations are optimistic; a failed server call is corrected by the next hydrate.
  const removeQueuedPrompt = useCallback(
    (id: string) => {
      dispatch({ kind: "dequeue_prompt", id });
      if (!sessionId) return;
      markQueuedRemoved(sessionId, [id]);
      void removeServerQueuedPrompt(sessionId, id);
    },
    [sessionId],
  );

  const editQueuedPrompt = useCallback(
    (id: string, text: string) => {
      dispatch({ kind: "edit_queued_prompt", id, text });
      if (!sessionId) return;
      markQueuedEdited(sessionId, id, text);
      const previous = queueEdits.get(sessionId);
      const send = () => editServerQueuedPrompt(sessionId, id, text);
      const edit = previous ? previous.then(send, send) : send();
      queueEdits.set(sessionId, edit);
      const settle = () => {
        if (queueEdits.get(sessionId) === edit) queueEdits.delete(sessionId);
      };
      edit.then(settle, settle);
    },
    [sessionId],
  );

  const clearQueue = useCallback(() => {
    dispatch({ kind: "clear_queue" });
    if (!sessionId) return;
    markQueuedRemoved(
      sessionId,
      queuedPromptsRef.current.map((q) => q.id),
    );
    void clearServerQueue(sessionId);
  }, [sessionId, queuedPromptsRef]);

  // Never bind this to Escape: an accidental press would abort work the user meant to keep.
  const hostPromptIntent = state.hostPromptIntent;
  const performCancelPrompt = useCallback(async (): Promise<string | null> => {
    if (!sessionId || !ownsLiveAcpActions(sessionId, actionOwner)) return NATIVE_INPUT_UNCONFIRMED;
    const current = inputStateRef.current;
    if (current.hostPromptIntent !== hostPromptIntent) return NATIVE_INPUT_UNCONFIRMED;
    const hostIntent =
      current.nativeSnapshotSeq != null &&
      Number.isFinite(current.nativeSnapshotSeq) &&
      current.connectionUnavailable === null
        ? current.hostPromptIntent
        : null;
    if (!hostIntent && !requireNativeInput()) return NATIVE_INPUT_UNCONFIRMED;
    const request: RequestInit = { method: "POST" };
    if (hostIntent) {
      request.headers = { "Content-Type": "application/json" };
      request.body = JSON.stringify({ host_prompt_intent: hostIntent });
    }
    const message = await postErrorMessage(acpUrl(sessionId, "cancel"), request, ["cancel", "cancelling"]);
    if (message && ownsLiveAcpActions(sessionId, actionOwner)) dispatch({ kind: "error", message });
    return message;
  }, [sessionId, actionOwner, hostPromptIntent, requireNativeInput]);
  const cancelPrompt = useCallback(async () => (await performCancelPrompt()) === null, [performCancelPrompt]);

  const performCancelSubagent = useCallback(
    async (childId: string): Promise<string | null> => {
      if (!sessionId || !ownsLiveAcpActions(sessionId, actionOwner)) return NATIVE_INPUT_UNCONFIRMED;
      if (!requireNativeInput()) return NATIVE_INPUT_UNCONFIRMED;
      const message = await cancelSubagent(sessionId, childId);
      if (message && ownsLiveAcpActions(sessionId, actionOwner)) dispatch({ kind: "error", message });
      return message;
    },
    [sessionId, actionOwner, requireNativeInput],
  );
  const cancelSubagentById = useCallback(
    async (childId: string) => {
      await performCancelSubagent(childId);
    },
    [performCancelSubagent],
  );

  useLayoutEffect(() => {
    if (!sessionId) return;
    return registerLiveAcpActions(sessionId, actionOwner, {
      cancelPrompt: performCancelPrompt,
      cancelSubagent: performCancelSubagent,
    });
  }, [sessionId, actionOwner, performCancelPrompt, performCancelSubagent]);

  const steerable = !!state.promptCapabilities?.steering && !state.cancelling && !state.compacting;

  // A non-steerable running turn is cancelled and the server drains the row instead.
  const sendQueuedNow = useCallback(
    async (prompt: QueuedPrompt) => {
      const sid = sessionIdRef.current;
      if (!sid) return;
      if (!requireNativeInput(undefined, "prompt_intent")) return;
      if (state.turnActive && !steerable) {
        await cancelPrompt();
        return;
      }
      // Removing a row whose bytes live only on the server would delete them before the send.
      if (prompt.attachments?.some((a) => !a.dataB64)) return;
      // The daemon consumes the row once it accepts the send, so the drain can't also deliver it
      // and a plugin-sent row keeps the sender it stored.
      dispatch({ kind: "dequeue_prompt", id: prompt.id });
      const result = await dispatchPromptNow(prompt.text, prompt.attachments, prompt.id);
      // A concurrent list can have reintroduced the row while delivery awaited
      // the daemon. Settle it again at acknowledgement before any retained-row upsert.
      dispatch({ kind: "dequeue_prompt", id: prompt.id });
      // The daemon keeps the row until it accepts the send, so a refusal or a re-park leaves it queued.
      if (result.kind === "queued") {
        showServerQueued(result.queuedId, prompt.text, prompt.attachments);
      } else if (
        result.kind === "retryable_failure" ||
        result.kind === "worker_not_ready" ||
        result.kind === "non_retryable_failure"
      ) {
        showServerQueued(prompt.id, prompt.text, prompt.attachments);
      }
    },
    [sessionIdRef, state.turnActive, steerable, cancelPrompt, dispatchPromptNow, showServerQueued, requireNativeInput],
  );

  // Pessimistic: the value changes when the adapter confirms; pending only dims the clicked option.
  const setConfigOption = useCallback(
    async (configId: string, value: string) => {
      if (!sessionId) return;
      if (!requireNativeInput()) return;
      dispatch({ kind: "set_pending_config_option", configId, value });
      const message = await postErrorMessage(
        acpUrl(sessionId, "config-option"),
        jsonPost({ config_id: configId, value }),
        [`set ${configId}`, `setting ${configId}`],
        () => dispatch({ kind: "clear_pending_config_option_if_match", configId, value }),
      );
      if (message) dispatch({ kind: "error", message });
    },
    [sessionId, requireNativeInput],
  );

  // The server publishes the resulting Stopped event; no client-side Stopped is fabricated.
  const hostOnlyForceRefused = hostPromptIntent != null && !(state.turnActive && nativeInputAvailable(state));
  const forceEndTurn = useCallback(async () => {
    if (!sessionId || !ownsLiveAcpActions(sessionId, actionOwner) || hostOnlyForceRefused) return;
    if (inputStateRef.current.hostPromptIntent !== hostPromptIntent) return;
    lastActivityRef.current = Date.now();
    const message = await postErrorMessage(acpUrl(sessionId, "force_end_turn"), { method: "POST" }, [
      "force end turn",
      "forcing end turn",
    ]);
    if (message && ownsLiveAcpActions(sessionId, actionOwner)) dispatch({ kind: "error", message });
  }, [sessionId, actionOwner, hostOnlyForceRefused, hostPromptIntent, inputStateRef, lastActivityRef]);

  const dismissers = useMemo(
    () => ({
      dismissError: () => dispatch({ kind: "clear_error" }),
      dismissPrimer: () => dispatch({ kind: "dismiss_primer" }),
      dismissCompactionReminder: () => dispatch({ kind: "dismiss_compaction_reminder" }),
      dismissRejectedPrompt: (id: string) => dispatch({ kind: "dismiss_rejected_prompt", id }),
      dismissModeSwitchFailed: () => dispatch({ kind: "dismiss_mode_switch_failed" }),
      dismissSessionNotice: (id: string) => dispatch({ kind: "dismiss_session_notice", id }),
      dismissConfigOptionSwitchFailed: () => dispatch({ kind: "dismiss_config_option_switch_failed" }),
      markRestartStarted: (epoch: number, stoppedSent: boolean, retriedError?: string) =>
        dispatch({ kind: "restart_started", epoch, stoppedSent, retriedError }),
      markRestartFailed: (epoch: number, message: string, stoppedSent: boolean) =>
        dispatch({ kind: "restart_failed", epoch, message, stoppedSent }),
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
    retryOperation,
    retryRejectedPrompt,
    cancelPrompt,
    cancelSubagent: cancelSubagentById,
    forceEndTurn,
    ...dismissers,
    removeQueuedPrompt,
    editQueuedPrompt,
    clearQueue,
    sendQueuedNow,
    canSendQueuedNow: status === "open" && canSubmitPromptIntent(state) && workerState !== "stopping",
    /** Send now would cancel a running, non-steerable turn rather than send immediately. */
    sendNowInterruptsTurn: state.turnActive && !steerable,
    setConfigOption,
  };
}
