// Client-side ACP reducer: control frames, the optimistic overlay, and the prompt queue.

import {
  appendElicitationAnswerRow,
  applyEvent,
  applyReducedState,
  deriveTurnActive,
  emptyAcpState,
  mergePrependedActivity,
  mergeServerRows,
  patchServerRow,
  reduceFrames,
  summarizeAnswers,
  transcriptRowToActivity,
  webRendersServerRow,
  type AcpAttachment,
  type AcpFrame,
  type AcpState,
  type ActivityRow,
  type ElicitationResolution,
  type PromptAttachmentInput,
  type QueuedPrompt,
  type ReducedState,
  type TranscriptDelta,
  type TranscriptRow,
} from "../../lib/acpTypes";
import type { ServerQueuedPrompt } from "../../lib/api";

export type Action =
  | { kind: "frame"; frame: AcpFrame }
  | { kind: "reduced_state"; state: ReducedState; unchanged: string[] }
  | { kind: "frames"; frames: AcpFrame[]; rows?: ActivityRow[]; oldestSeq?: number }
  | { kind: "prepend"; rows: ActivityRow[]; oldestSeq: number }
  | { kind: "handshake"; frames: AcpFrame[] }
  | { kind: "transcript_snapshot"; rows: ActivityRow[] }
  | { kind: "transcript_append"; row: ActivityRow }
  | { kind: "transcript_patch"; row: ActivityRow }
  | { kind: "transcript_remove"; id: string }
  | { kind: "lagged"; skipped: number }
  | { kind: "user_prompt"; text: string; attachments?: AcpAttachment[]; id?: string }
  | { kind: "prompt_send_rejected"; id: string }
  | { kind: "settle_inflight_prompt"; id: string }
  | { kind: "rollback_optimistic_prompt"; id: string }
  | { kind: "error"; message: string }
  | { kind: "clear_error" }
  | { kind: "approval_resolved_locally"; nonce: string }
  | { kind: "elicitation_resolved_locally"; nonce: string; resolution: ElicitationResolution }
  | { kind: "lagged_resolved" }
  | { kind: "reset" }
  | { kind: "hydrate"; state: AcpState }
  | { kind: "enqueue_prompt"; id: string; text: string; attachments?: PromptAttachmentInput[] }
  | { kind: "dequeue_prompt"; id: string }
  | { kind: "edit_queued_prompt"; id: string; text: string }
  | { kind: "clear_queue" }
  | { kind: "hydrate_server_queue"; rows: ServerQueuedPrompt[] }
  | { kind: "confirm_queued_prompt"; id: string }
  | { kind: "dismiss_primer" }
  | { kind: "dismiss_compaction_reminder" }
  | { kind: "dismiss_rejected_prompt"; id: string }
  | { kind: "dismiss_mode_switch_failed" }
  | { kind: "set_pending_config_option"; configId: string; value: string }
  | { kind: "clear_pending_config_option" }
  // Only clears when still matching, so a stale failed request can't wipe a newer click.
  | { kind: "clear_pending_config_option_if_match"; configId: string; value: string }
  | { kind: "dismiss_config_option_switch_failed" };

export type ApprovalResolveOutcome = { kind: "resolved" } | { kind: "error"; message: string };

/** A 2xx, or a 404 naming this nonce (already resolved server-side), clears the card; anything else is an error. */
export function classifyResolveResponse(
  target: "approval" | "elicitation",
  ok: boolean,
  status: number,
  detail: string,
  nonce: string,
): ApprovalResolveOutcome {
  if (ok) return { kind: "resolved" };
  if (status === 404 && detail.toLowerCase().includes(`no pending ${target}`) && detail.includes(nonce)) {
    return { kind: "resolved" };
  }
  const noun = target === "approval" ? "approval" : "question";
  return { kind: "error", message: `Could not resolve ${noun} (${status}). ${detail}`.trim() };
}

export function toActivityRows(rows: readonly TranscriptRow[], sessionId: string): ActivityRow[] {
  return rows.filter(webRendersServerRow).map((r) => transcriptRowToActivity(r, sessionId));
}

function settleInflightPrompt(state: AcpState, id: string): AcpState {
  const inflightPromptIds = state.inflightPromptIds.filter((p) => p !== id);
  if (inflightPromptIds.length === state.inflightPromptIds.length) return state;
  return {
    ...state,
    inflightPromptIds,
    turnActive: deriveTurnActive({ serverTurnActive: state.serverTurnActive, inflightPromptIds }),
  };
}

/** Drop overlay rows whose same-id server row has landed in `activity`. */
function pruneOptimisticRows(state: AcpState): AcpState {
  if (state.optimisticRows.length === 0) return state;
  const serverIds = new Set(state.activity.map((r) => r.id));
  const kept = state.optimisticRows.filter((o) => !serverIds.has(o.id));
  if (kept.length === state.optimisticRows.length) return state;
  return { ...state, optimisticRows: kept };
}

function withServerRows(state: AcpState, activity: ActivityRow[]): AcpState {
  return pruneOptimisticRows({ ...state, activity });
}

function mapQueued(state: AcpState, id: string, update: (q: QueuedPrompt) => QueuedPrompt): AcpState {
  return { ...state, queuedPrompts: state.queuedPrompts.map((q) => (q.id === id ? update(q) : q)) };
}

// Keeps local attachment bytes for rows queued here, builds metadata-only views for the rest,
// and keeps optimistic rows whose enqueue POST has not landed yet.
function hydrateServerQueue(state: AcpState, serverRows: ServerQueuedPrompt[]): AcpState {
  const rows = Array.isArray(serverRows) ? serverRows : [];
  const serverIds = new Set(rows.map((r) => r.id));
  const localById = new Map(state.queuedPrompts.map((q) => [q.id, q]));
  const merged: QueuedPrompt[] = rows.map((r) => {
    const local = localById.get(r.id);
    const attachments: PromptAttachmentInput[] | undefined = local?.attachments?.length
      ? local.attachments
      : r.attachments && r.attachments.length > 0
        ? r.attachments.map((a) => ({ kind: a.kind, mimeType: a.mime_type, name: a.name ?? undefined, dataB64: "" }))
        : undefined;
    return {
      id: r.id,
      text: r.text,
      queuedAt: r.created_at || local?.queuedAt || new Date().toISOString(),
      ...(attachments ? { attachments } : {}),
    };
  });
  const stillPending = state.queuedPrompts.filter((q) => q.pending && !serverIds.has(q.id));
  return { ...state, queuedPrompts: merged.concat(stillPending) };
}

export function reducer(state: AcpState, action: Action): AcpState {
  switch (action.kind) {
    case "frame":
      return applyEvent(state, action.frame);
    case "reduced_state":
      return applyReducedState(state, action.state, action.unchanged);
    case "frames": {
      // Frames feed control state; the transcript comes from the server-folded rows.
      let next = action.frames.reduce(applyEvent, state);
      if (action.rows && action.rows.length > 0)
        next = withServerRows(next, mergeServerRows(next.activity, action.rows));
      if (action.oldestSeq != null && state.oldestSeq === 0) return { ...next, oldestSeq: action.oldestSeq };
      return next;
    }
    case "prepend": {
      // Older history only adds rows; control state is not a pure fold and must not be touched.
      const next = { ...state, oldestSeq: action.oldestSeq };
      if (action.rows.length === 0) return next;
      next.activity = mergePrependedActivity(action.rows, state.activity);
      return next;
    }
    case "transcript_snapshot":
      if (action.rows.length === 0) return state;
      return withServerRows(state, mergeServerRows(state.activity, action.rows));
    case "transcript_append":
      return withServerRows(state, mergeServerRows(state.activity, [action.row]));
    case "transcript_patch":
      return withServerRows(state, patchServerRow(state.activity, action.row));
    case "transcript_remove": {
      const activity = state.activity.filter((r) => r.id !== action.id);
      return activity.length === state.activity.length ? state : { ...state, activity };
    }
    case "handshake": {
      // A recent-first open skips the seq-0 handshake; backfill only fields still at their default.
      const hs = reduceFrames(action.frames);
      return {
        ...state,
        agent: state.agent ?? hs.agent,
        model: state.model ?? hs.model,
        mode: state.mode !== "Default" ? state.mode : hs.mode,
        promptCapabilities: state.promptCapabilities ?? hs.promptCapabilities,
        availableModes: state.availableModes.length > 0 ? state.availableModes : hs.availableModes,
        currentModeId: state.currentModeId ?? hs.currentModeId,
        availableCommands: state.availableCommands.length > 0 ? state.availableCommands : hs.availableCommands,
        configOptions: state.configOptions.length > 0 ? state.configOptions : hs.configOptions,
      };
    }
    case "lagged":
      return { ...state, lagged: true };
    case "lagged_resolved":
      return { ...state, lagged: false };
    case "error":
      return { ...state, lastError: action.message };
    case "clear_error":
      return { ...state, lastError: null };
    case "approval_resolved_locally": {
      // Don't wait for the broadcast, which the seq dedupe can swallow (#1821).
      const pendingApprovals = state.pendingApprovals.filter((a) => a.nonce !== action.nonce);
      const removed = pendingApprovals.length !== state.pendingApprovals.length;
      return {
        ...state,
        lastError: removed ? null : state.lastError,
        pendingApprovals,
        locallyResolved: [...state.locallyResolved, action.nonce],
      };
    }
    case "elicitation_resolved_locally": {
      const card = state.pendingElicitations.find((e) => e.nonce === action.nonce);
      const pendingElicitations = state.pendingElicitations.filter((e) => e.nonce !== action.nonce);
      const removed = pendingElicitations.length !== state.pendingElicitations.length;
      const answers =
        card && action.resolution.action === "accept" ? summarizeAnswers(card, action.resolution.answers) : [];
      return {
        ...state,
        lastError: removed ? null : state.lastError,
        pendingElicitations,
        locallyResolved: [...state.locallyResolved, action.nonce],
        optimisticRows: appendElicitationAnswerRow(state.optimisticRows, action.nonce, answers),
      };
    }
    case "hydrate":
      return action.state;
    case "user_prompt": {
      // Overlay row keyed by the POSTed prompt_id; the server's same-id row replaces it.
      const id = action.id ?? `user-opt-${Date.now()}-${state.optimisticRows.length}`;
      const row: ActivityRow = {
        id,
        kind: "user_prompt",
        text: action.text,
        attachments: action.attachments && action.attachments.length > 0 ? action.attachments : undefined,
        at: new Date().toISOString(),
      };
      return {
        ...state,
        optimisticRows: state.optimisticRows.concat(row),
        startupError: null,
        lastError: null,
        inflightPromptIds: state.inflightPromptIds.includes(id)
          ? state.inflightPromptIds
          : state.inflightPromptIds.concat(id),
        promptSeq: state.promptSeq + 1,
        turnActive: true,
      };
    }
    case "prompt_send_rejected":
      // The overlay row stays so the user sees what they tried to send.
      return settleInflightPrompt({ ...state, inFlightTool: null }, action.id);
    case "settle_inflight_prompt":
      return settleInflightPrompt(state, action.id);
    case "rollback_optimistic_prompt": {
      // The prompt now lives only in the queue; a leftover overlay row would duplicate its resend.
      const idx = state.optimisticRows.findIndex((r) => r.id === action.id);
      if (idx === -1) return settleInflightPrompt(state, action.id);
      const optimisticRows = state.optimisticRows.slice(0, idx).concat(state.optimisticRows.slice(idx + 1));
      return settleInflightPrompt({ ...state, optimisticRows }, action.id);
    }
    case "enqueue_prompt": {
      const entry: QueuedPrompt = {
        id: action.id,
        text: action.text,
        queuedAt: new Date().toISOString(),
        pending: true,
        ...(action.attachments && action.attachments.length > 0 ? { attachments: action.attachments } : {}),
      };
      return { ...state, queuedPrompts: state.queuedPrompts.concat(entry) };
    }
    case "dequeue_prompt":
      return { ...state, queuedPrompts: state.queuedPrompts.filter((q) => q.id !== action.id) };
    case "edit_queued_prompt":
      return mapQueued(state, action.id, (q) => ({ ...q, text: action.text }));
    case "clear_queue":
      return { ...state, queuedPrompts: [] };
    case "hydrate_server_queue":
      return hydrateServerQueue(state, action.rows);
    case "confirm_queued_prompt":
      return mapQueued(state, action.id, (q) => ({ ...q, pending: false }));
    case "dismiss_primer":
      return { ...state, contextPrimerAvailable: null };
    case "dismiss_compaction_reminder":
      return { ...state, compactionReminderDismissed: state.sessionUsage };
    case "dismiss_rejected_prompt":
      return { ...state, rejectedPrompts: state.rejectedPrompts.filter((r) => r.id !== action.id) };
    case "dismiss_mode_switch_failed":
      return { ...state, modeSwitchFailed: null };
    case "set_pending_config_option":
      return { ...state, pendingConfigOption: { configId: action.configId, value: action.value } };
    case "clear_pending_config_option":
      return { ...state, pendingConfigOption: null };
    case "clear_pending_config_option_if_match": {
      const pending = state.pendingConfigOption;
      const matches = pending?.configId === action.configId && pending?.value === action.value;
      return matches ? { ...state, pendingConfigOption: null } : state;
    }
    case "dismiss_config_option_switch_failed":
      return { ...state, configOptionSwitchFailed: null };
    default:
      return emptyAcpState();
  }
}

export function transcriptDeltaAction(delta: TranscriptDelta, sessionId: string): Action | null {
  if ("Append" in delta) {
    if (!webRendersServerRow(delta.Append)) return null;
    return { kind: "transcript_append", row: transcriptRowToActivity(delta.Append, sessionId) };
  }
  if ("Patch" in delta) {
    if (!webRendersServerRow(delta.Patch.row)) return null;
    return { kind: "transcript_patch", row: transcriptRowToActivity(delta.Patch.row, sessionId) };
  }
  if ("Remove" in delta) return { kind: "transcript_remove", id: delta.Remove };
  return null;
}
