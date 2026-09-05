// Structured view subscription hook.
//
// Connects to /sessions/{id}/acp/ws, receives AcpBroadcastFrame
// JSON, and reduces them into a AcpState. On `lagged` notices the
// hook hits the snapshot endpoint to recover any missed frames before
// resuming live broadcast. Errors from sendPrompt / resolveApproval /
// cancelPrompt are surfaced via state.lastError so the user gets a
// dismissible banner instead of a silently-lost action.

import { useCallback, useEffect, useReducer, useRef, useState, useSyncExternalStore } from "react";
import {
  appendElicitationAnswerRow,
  applyEvent,
  applyReducedState,
  emptyAcpState,
  deriveTurnActive,
  mergePrependedActivity,
  mergeServerRows,
  normaliseTurnState,
  patchServerRow,
  reduceFrames,
  summarizeAnswers,
  transcriptRowToActivity,
  webRendersServerRow,
  type ActivityRow,
  type ApprovalDecision,
  type AcpAttachment,
  type AcpFrame,
  type AcpState,
  type BackgroundAgent,
  type ElicitationResolution,
  type PromptAttachmentInput,
  type QueuedPrompt,
  type ReducedState,
  type TranscriptDelta,
  type TranscriptRow,
} from "../lib/acpTypes";
import { getOrCreateDeviceBindingSecret } from "../lib/deviceBinding";
import { safeSetItem } from "../lib/safeStorage";
import {
  STORAGE_KEY_PREFIX,
  STATE_TTL_MS,
  clearQueueCount,
  setQueueCount,
  setRateLimit,
  type PersistedEntry,
} from "../lib/acpStateStorage";
import { getToken } from "../lib/token";
import {
  clearServerQueue,
  editServerQueuedPrompt,
  enqueueServerPrompt,
  listServerQueue,
  removeServerQueuedPrompt,
  reportAcpInteraction,
  setSessionArchive,
  setSessionSnooze,
  type ServerQueuedPrompt,
} from "../lib/api";

/** Outcome of an immediate prompt POST, used by the drain effect to
 *  decide whether to retire queued items (delivered or permanently
 *  rejected) or keep them for a later retry (transient failure). */
type PromptSendResult =
  /** The daemon started a fresh turn or steered the prompt into the running
   *  one. Either way the optimistic transcript row stands. */
  | { kind: "dispatched" }
  /** The daemon parked it and returned the server queue row id. */
  | { kind: "queued"; queuedId: string }
  | { kind: "retryable_failure" }
  | { kind: "non_retryable_failure" };

/** Wire shape of the `/acp/prompt` 202 body (Rust `PromptDispatchResponse`). */
interface PromptDispatchBody {
  disposition?: "sent" | "steered" | "queued";
  queued_id?: string;
}

export type Action =
  | { kind: "frame"; frame: AcpFrame }
  /** The daemon's folded control state, adopted verbatim (Tier 1.2). */
  | { kind: "reduced_state"; state: ReducedState; unchanged: string[] }
  | { kind: "frames"; frames: AcpFrame[]; rows?: ActivityRow[]; oldestSeq?: number }
  | { kind: "prepend"; rows: ActivityRow[]; oldestSeq: number }
  | { kind: "handshake"; frames: AcpFrame[] }
  /** Full server-folded row list from a WS `transcript_snapshot` (usually a
   *  no-op since the WS dials at the current lastSeq; carries the gap rows on
   *  a reconnect-with-events). Merged by id. */
  | { kind: "transcript_snapshot"; rows: ActivityRow[] }
  /** A live `transcript_delta` Append: a new server row (merged by id, so a
   *  re-delivered append is idempotent). */
  | { kind: "transcript_append"; row: ActivityRow }
  /** A live `transcript_delta` Patch: replace the row by id with the server's
   *  authoritative new row. */
  | { kind: "transcript_patch"; row: ActivityRow }
  /** A live `transcript_delta` Remove: drop the row by id (an AskUserQuestion
   *  tool card superseded by its elicitation form). */
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
  | {
      kind: "enqueue_prompt";
      /** Caller-minted stable id, so the enqueue POST + confirm can target
       *  this exact optimistic row and the server row reconciles against it. */
      id: string;
      text: string;
      attachments?: PromptAttachmentInput[];
    }
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
  | {
      /** Clear pendingConfigOption only when it still matches the
       *  (configId, value) pair of the failed request. Prevents a
       *  stale request A from wiping a newer request B's pending
       *  state after the user clicked a second option mid-flight.
       *  See #1403 (review feedback). */
      kind: "clear_pending_config_option_if_match";
      configId: string;
      value: string;
    }
  | { kind: "dismiss_config_option_switch_failed" };

// Per-session memory and localStorage cache. Versioned keys allow schema
// invalidation; TTL and LRU limits bound abandoned session state. Refresh an
// existing Map key with delete then set because Map.set preserves its order.
const STATE_CACHE_CAP = 32;
const stateCache = new Map<string, AcpState>();

function storageKey(sessionId: string): string {
  return STORAGE_KEY_PREFIX + sessionId;
}

// Walk `aoe:acp-state:v1:*` keys and remove the single oldest one
// (by `savedAt`), preferring corrupt entries when present. Returns true
// when an entry was removed so the caller can retry the write. The
// whitelist filter is load-bearing: it must never touch `acp:draft:*`
// or any unrelated key. Drafts are authoritative client-side state and
// cross-tab subscribers observe their removal immediately, so silently
// evicting them would be data loss (see #1345 debate).
function evictOldestPersistedAcpState(currentKey: string): boolean {
  if (typeof window === "undefined") return false;
  try {
    let oldestKey: string | null = null;
    let oldestTime = Infinity;
    let firstCorruptKey: string | null = null;
    for (let i = 0; i < window.localStorage.length; i++) {
      const k = window.localStorage.key(i);
      if (!k || !k.startsWith(STORAGE_KEY_PREFIX)) continue;
      if (k === currentKey) continue;
      const raw = window.localStorage.getItem(k);
      if (raw === null) continue;
      try {
        const parsed = JSON.parse(raw) as PersistedEntry | null;
        if (!parsed || typeof parsed.savedAt !== "number" || Number.isNaN(parsed.savedAt)) {
          if (firstCorruptKey === null) firstCorruptKey = k;
          continue;
        }
        if (parsed.savedAt < oldestTime) {
          oldestTime = parsed.savedAt;
          oldestKey = k;
        }
      } catch {
        if (firstCorruptKey === null) firstCorruptKey = k;
      }
    }
    const victim = firstCorruptKey ?? oldestKey;
    if (!victim) return false;
    window.localStorage.removeItem(victim);
    return true;
  } catch {
    return false;
  }
}

/** Project the in-memory reducer state into the shape written to
 *  localStorage. Queued prompts that carry attachments are dropped
 *  entirely: their base64 bytes would blow the per-origin quota, and
 *  persisting the text alone would silently drain a degraded prompt on
 *  reload (e.g. "fix this screenshot:" with no screenshot). The full
 *  row stays in the in-memory `stateCache`, so it survives a component
 *  remount but not a hard page reload. See #1833 / #1000. */
function toPersistedState(state: AcpState): AcpState {
  // Optimistic overlay rows are ephemeral client presentation: a confirmed
  // prompt is already in the server-owned `activity` (re-fetched on reload),
  // so persisting the overlay would risk a stale duplicate. Drop it, and
  // with it the in-flight prompt ids: after a reload no POST is left to
  // acknowledge them, so a persisted id would latch the spinner forever.
  const base: AcpState =
    state.optimisticRows.length > 0 || state.inflightPromptIds.length > 0
      ? { ...state, optimisticRows: [], inflightPromptIds: [] }
      : state;
  if (!base.queuedPrompts.some((q) => q.attachments?.length)) return base;
  return {
    ...base,
    queuedPrompts: base.queuedPrompts.filter((q) => !q.attachments?.length),
  };
}

function persistState(sessionId: string, state: AcpState): void {
  const key = storageKey(sessionId);
  const body = JSON.stringify({
    savedAt: Date.now(),
    state: toPersistedState(state),
  } satisfies PersistedEntry);
  if (safeSetItem(key, body)) {
    setQueueCount(sessionId, state.queuedPrompts.length);
    setRateLimit(sessionId, state.rateLimit);
    return;
  }
  // Storage write failed (likely QuotaExceeded). Evict a single oldest
  // structured view cache entry and retry exactly once. On a second failure the
  // cache is best-effort: the next reload replays from the server, so
  // we stay silent here per the deliberate UX choice for cache writes.
  if (!evictOldestPersistedAcpState(key)) return;
  if (safeSetItem(key, body)) {
    setQueueCount(sessionId, state.queuedPrompts.length);
    setRateLimit(sessionId, state.rateLimit);
  }
}

// Test-only exports so the eviction policy can be exercised without
// driving the full hook lifecycle. Not part of the public API.
export const __test = {
  persistState,
  loadPersistedState,
  evictOldestPersistedAcpState,
  STORAGE_KEY_PREFIX,
};

function loadPersistedState(sessionId: string): AcpState | undefined {
  if (typeof window === "undefined") return undefined;
  try {
    const raw = window.localStorage.getItem(storageKey(sessionId));
    if (!raw) return undefined;
    const parsed = JSON.parse(raw) as PersistedEntry | null;
    if (!parsed || typeof parsed.savedAt !== "number" || typeof parsed.state !== "object" || parsed.state === null) {
      return undefined;
    }
    if (Date.now() - parsed.savedAt > STATE_TTL_MS) {
      window.localStorage.removeItem(storageKey(sessionId));
      return undefined;
    }
    const state = parsed.state as Partial<AcpState>;
    if (typeof state.lastSeq !== "number" || !Array.isArray(state.activity) || !Array.isArray(state.queuedPrompts)) {
      window.localStorage.removeItem(storageKey(sessionId));
      return undefined;
    }
    // Merge over the current defaults so an entry persisted by an older
    // bundle gains any fields added since (e.g. `pendingElicitations`);
    // without this the new code reads `undefined` for a freshly-added
    // array and crashes on `.map`. Then backfill the turn state; see
    // `normaliseTurnState` for the rules.
    const merged: AcpState = { ...emptyAcpState(), ...(state as AcpState) };
    return normaliseTurnState(merged);
  } catch {
    return undefined;
  }
}

function dropPersistedState(sessionId: string): void {
  if (typeof window === "undefined") return;
  try {
    window.localStorage.removeItem(storageKey(sessionId));
  } catch {
    // ignore
  }
}

function dropAllPersistedState(): void {
  if (typeof window === "undefined") return;
  try {
    const toRemove: string[] = [];
    for (let i = 0; i < window.localStorage.length; i++) {
      const k = window.localStorage.key(i);
      if (k && k.startsWith(STORAGE_KEY_PREFIX)) toRemove.push(k);
    }
    for (const k of toRemove) window.localStorage.removeItem(k);
  } catch {
    // ignore
  }
}

let sweptStorage = false;
function sweepExpiredStorage(): void {
  if (sweptStorage) return;
  sweptStorage = true;
  if (typeof window === "undefined") return;
  try {
    const toRemove: string[] = [];
    const now = Date.now();
    for (let i = 0; i < window.localStorage.length; i++) {
      const k = window.localStorage.key(i);
      if (!k || !k.startsWith(STORAGE_KEY_PREFIX)) continue;
      const raw = window.localStorage.getItem(k);
      if (!raw) {
        toRemove.push(k);
        continue;
      }
      try {
        const parsed = JSON.parse(raw) as PersistedEntry | null;
        if (!parsed || typeof parsed.savedAt !== "number" || now - parsed.savedAt > STATE_TTL_MS) {
          toRemove.push(k);
        }
      } catch {
        toRemove.push(k);
      }
    }
    for (const k of toRemove) window.localStorage.removeItem(k);
  } catch {
    // ignore
  }
}

function cacheGet(sessionId: string): AcpState | undefined {
  const value = stateCache.get(sessionId);
  if (value !== undefined) {
    // Touch the LRU position by re-inserting at the back of the Map's
    // insertion order.
    stateCache.delete(sessionId);
    stateCache.set(sessionId, value);
    return value;
  }
  const persisted = loadPersistedState(sessionId);
  if (persisted !== undefined) {
    stateCache.set(sessionId, persisted);
    while (stateCache.size > STATE_CACHE_CAP) {
      const oldest = stateCache.keys().next().value;
      if (oldest === undefined) break;
      stateCache.delete(oldest);
    }
    // A `useBackgroundAgents` subscriber that already rendered an empty
    // snapshot (panel open before this cache was primed) won't see the
    // persisted agents until something else calls `cacheSet`. Wake it.
    // Deferred so the notify never fires during a render that called
    // `cacheGet` (e.g. the reducer initializer).
    queueMicrotask(() => notifyStateListeners(sessionId));
    return persisted;
  }
  return undefined;
}

function cacheSet(sessionId: string, value: AcpState): void {
  stateCache.delete(sessionId);
  stateCache.set(sessionId, value);
  while (stateCache.size > STATE_CACHE_CAP) {
    const oldest = stateCache.keys().next().value;
    if (oldest === undefined) break;
    stateCache.delete(oldest);
  }
  persistState(sessionId, value);
  notifyStateListeners(sessionId);
}

// Lightweight per-session subscription over `stateCache` so a component
// that is NOT a child of <StructuredView> (the Background agents panel
// lives in a sibling Dock) can read derived ACP state without opening a
// second WebSocket. Notified on every `cacheSet`; consumers diff by
// reference in their `getSnapshot`, so a no-op change costs nothing.
const stateListeners = new Map<string, Set<() => void>>();

function notifyStateListeners(sessionId: string): void {
  const set = stateListeners.get(sessionId);
  if (!set) return;
  for (const cb of set) cb();
}

/** Subscribe to ACP-state changes for `sessionId`. Returns an unsubscribe. */
function subscribeAcpState(sessionId: string, cb: () => void): () => void {
  let set = stateListeners.get(sessionId);
  if (!set) {
    set = new Set();
    stateListeners.set(sessionId, set);
  }
  set.add(cb);
  return () => {
    const s = stateListeners.get(sessionId);
    if (!s) return;
    s.delete(cb);
    if (s.size === 0) stateListeners.delete(sessionId);
  };
}

/** Non-mutating peek at cached state (no LRU touch; safe in getSnapshot). */
function peekAcpState(sessionId: string): AcpState | undefined {
  return stateCache.get(sessionId);
}

const EMPTY_BACKGROUND_AGENTS: BackgroundAgent[] = [];

/** Read the live background-agents list for a session. Sibling-safe (no
 *  extra WebSocket); reuses the single subscription <StructuredView>
 *  already holds. Re-renders only when the list reference changes. */
export function useBackgroundAgents(sessionId: string | null): BackgroundAgent[] {
  const subscribe = useCallback(
    (cb: () => void) => (sessionId ? subscribeAcpState(sessionId, cb) : () => {}),
    [sessionId],
  );
  const getSnapshot = useCallback(
    () =>
      sessionId ? (peekAcpState(sessionId)?.backgroundAgents ?? EMPTY_BACKGROUND_AGENTS) : EMPTY_BACKGROUND_AGENTS,
    [sessionId],
  );
  return useSyncExternalStore(subscribe, getSnapshot);
}

/** Drop a session's cached state (or the entire cache when called
 *  with no argument). Call from the session-delete handler so the
 *  next session created with the same id doesn't briefly show the
 *  prior transcript on remount. */
/** How far back of a seq overlap `fetchReplay` requests on every call.
 *  Catches events that landed in the broadcast tail without being
 *  applied by the reducer (e.g. WS connect drain races against the
 *  REST replay call). The reducer's `frame.seq <= state.lastSeq`
 *  dedupe makes the overlap idempotent. See #1100. */
const REPLAY_OVERLAP = 50;

/** Page size `fetchReplay` requests per call. The server paginates the
 *  replay endpoint and bounds its own page; this stays at or under that
 *  bound so a long session loads over several requests instead of one
 *  giant response. The loop follows `next_cursor` while `has_more`. */
const REPLAY_PAGE_SIZE = 1000;

/** `before` sentinel for the recent-first tail request: any value above
 *  every real seq makes the backend return the most recent page. The
 *  server clamps to i64, so MAX_SAFE_INTEGER is comfortably "newest".
 *  See #2236. */
const TAIL_BEFORE = Number.MAX_SAFE_INTEGER;

/** Frames pulled from seq 0 on a long-session cold open purely to project
 *  the handshake snapshot (capabilities, slash palette, agent/model);
 *  small because the handshake fires in the first few events. See #2236. */
const HANDSHAKE_PREFIX_SIZE = 50;

/** Shape of the `acp/replay` JSON response (forward and backward modes).
 *  `rows` is present only when the request passed `?view=rows` (the
 *  server-folded transcript rows for the page; `frames` is empty in that
 *  case). See `ReplayResponse` in src/acp/protocol.rs. */
type ReplayPageResponse = {
  frames: AcpFrame[];
  rows?: TranscriptRow[] | null;
  lost: boolean;
  highest_seq: number;
  next_cursor?: number | null;
  has_more?: boolean;
};

export function clearAcpCache(sessionId?: string): void {
  if (sessionId === undefined) {
    stateCache.clear();
    dropAllPersistedState();
    clearQueueCount();
  } else {
    stateCache.delete(sessionId);
    dropPersistedState(sessionId);
    clearQueueCount(sessionId);
  }
}

function initialState(sessionId: string | null): AcpState {
  if (!sessionId) return emptyAcpState();
  return cacheGet(sessionId) ?? emptyAcpState();
}

export function acpHookReducer(state: AcpState, action: Action): AcpState {
  return reducer(state, action);
}

/** Outcome of an approval-resolve POST: the card should clear, or an
 *  error banner should show. Pure so it can be unit-tested without the
 *  full hook. See #1821. */
export type ApprovalResolveOutcome = { kind: "resolved" } | { kind: "error"; message: string };

/** Classify an approval-resolve response. A 204 (ok) or a 404 whose body
 *  names *this* nonce both mean "this card is done" and clear it; any other
 *  failure (a session-gone 404, or a 404 that doesn't name the nonce)
 *  surfaces an error. Matching the nonce keeps a generic 404 from silently
 *  clearing the clicked card. See #1821. */
export function classifyApprovalResolveResponse(
  ok: boolean,
  status: number,
  detail: string,
  nonce: string,
): ApprovalResolveOutcome {
  if (ok) return { kind: "resolved" };
  if (status === 404 && /no pending approval/i.test(detail) && detail.includes(nonce)) {
    return { kind: "resolved" };
  }
  return {
    kind: "error",
    message: `Could not resolve approval (${status}). ${detail}`.trim(),
  };
}

/** Classify an elicitation-resolve response. Mirrors
 *  `classifyApprovalResolveResponse`: a 204 or a 404 naming *this* nonce
 *  (the question already resolved or was torn down server-side) both clear
 *  the card; anything else surfaces an error. */
export function classifyElicitationResolveResponse(
  ok: boolean,
  status: number,
  detail: string,
  nonce: string,
): ApprovalResolveOutcome {
  if (ok) return { kind: "resolved" };
  if (status === 404 && /no pending elicitation/i.test(detail) && detail.includes(nonce)) {
    return { kind: "resolved" };
  }
  return {
    kind: "error",
    message: `Could not resolve question (${status}). ${detail}`.trim(),
  };
}

/** Drop optimistic overlay rows whose server counterpart has landed in
 *  `activity` (same deterministic id), so the overlay never double-renders a
 *  confirmed prompt / elicitation answer. Returns `state` unchanged when
 *  nothing was pruned. */
/** Drop one in-flight prompt id and re-derive `turnActive`. Called for every
 *  POST outcome that no `UserPromptSent` echo will follow. */
function settleInflightPrompt(state: AcpState, id: string): AcpState {
  const inflightPromptIds = state.inflightPromptIds.filter((p) => p !== id);
  if (inflightPromptIds.length === state.inflightPromptIds.length) return state;
  return {
    ...state,
    inflightPromptIds,
    turnActive: deriveTurnActive({ serverTurnActive: state.serverTurnActive, inflightPromptIds }),
  };
}

function pruneOptimisticRows(state: AcpState): AcpState {
  if (state.optimisticRows.length === 0) return state;
  const serverIds = new Set(state.activity.map((r) => r.id));
  const kept = state.optimisticRows.filter((o) => !serverIds.has(o.id));
  if (kept.length === state.optimisticRows.length) return state;
  return { ...state, optimisticRows: kept };
}

export function reducer(state: AcpState, action: Action): AcpState {
  if (action.kind === "frame") {
    return applyEvent(state, action.frame);
  }
  if (action.kind === "reduced_state") {
    return applyReducedState(state, action.state, action.unchanged);
  }
  if (action.kind === "frames") {
    // Reduce the raw frames for CONTROL state (turn/approvals/usage/modes),
    // then merge the server-folded rows into the transcript (activity). The
    // transcript is server-owned (Tier 4); `applyEvent` no longer builds it.
    let next = action.frames.reduce(applyEvent, state);
    if (action.rows && action.rows.length > 0) {
      next = { ...next, activity: mergeServerRows(next.activity, action.rows) };
      next = pruneOptimisticRows(next);
    }
    // The recent-first tail load passes the page's lowest seq so the first
    // forward fold seeds the older-history watermark. Live WS batches omit it
    // (they append newer rows, never lower the floor).
    if (action.oldestSeq != null && state.oldestSeq === 0) {
      return { ...next, oldestSeq: action.oldestSeq };
    }
    return next;
  }
  if (action.kind === "prepend") {
    // Older history page: prepend its server-folded rows only; never touch
    // control state (optimistic prompt overlay, locally-resolved approvals
    // #1821, the prompt queue, pendingConfigOption are not a pure fold and
    // would be clobbered). Backward paging guarantees the page starts at a
    // turn boundary and ends just below the current oldest, so the only seam
    // artifact is a tool call whose real `tool_start` sits in this older page
    // while a synth start was already in the tail (the server's per-page
    // `?view=rows` fold synthesizes it); `mergePrependedActivity` merges the
    // real start into the tail row rather than emitting a duplicate id that
    // crashes assistant-ui's useResources ("Duplicate key"). See #2236 / #2711.
    const next = { ...state, oldestSeq: action.oldestSeq };
    if (action.rows.length === 0) return next;
    next.activity = mergePrependedActivity(action.rows, state.activity);
    return next;
  }
  if (action.kind === "transcript_snapshot") {
    // WS connect snapshot: the server folds events after our `since`, so this
    // is usually empty (the WS dials at the current lastSeq) and carries the
    // gap rows only on a reconnect that raced live events. Merge by id.
    if (action.rows.length === 0) return state;
    return pruneOptimisticRows({ ...state, activity: mergeServerRows(state.activity, action.rows) });
  }
  if (action.kind === "transcript_append") {
    return pruneOptimisticRows({ ...state, activity: mergeServerRows(state.activity, [action.row]) });
  }
  if (action.kind === "transcript_patch") {
    return pruneOptimisticRows({ ...state, activity: patchServerRow(state.activity, action.row) });
  }
  if (action.kind === "transcript_remove") {
    const activity = state.activity.filter((r) => r.id !== action.id);
    if (activity.length === state.activity.length) return state;
    return { ...state, activity };
  }
  if (action.kind === "handshake") {
    // Recent-first cold open skips the seq-0 handshake on a long session,
    // so the composer would have null capabilities and an empty slash
    // palette. Project the pinned handshake/snapshot frames (#1049) and
    // backfill ONLY the fields still at their empty default, so the tail's
    // authoritative recent values (e.g. a later model/mode switch) win and
    // no transcript rows are added (avoiding a detached island above the
    // gap). See #2236.
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
  if (action.kind === "lagged") {
    return { ...state, lagged: true };
  }
  if (action.kind === "lagged_resolved") {
    return { ...state, lagged: false };
  }
  if (action.kind === "error") {
    return { ...state, lastError: action.message };
  }
  if (action.kind === "clear_error") {
    return { ...state, lastError: null };
  }
  if (action.kind === "approval_resolved_locally") {
    // Optimistically drop the approval card once the server has accepted
    // the decision (204) or reports the nonce already gone (404), instead
    // of waiting on the ApprovalResolved broadcast, which the seq dedupe
    // can swallow and leave the card stuck. See #1821.
    const pendingApprovals = state.pendingApprovals.filter((a) => a.nonce !== action.nonce);
    // Only clear the error banner when a card was actually removed, so a
    // duplicate or stale action can't quietly hide an unrelated error.
    const removed = pendingApprovals.length !== state.pendingApprovals.length;
    return {
      ...state,
      lastError: removed ? null : state.lastError,
      pendingApprovals,
      locallyResolved: [...state.locallyResolved, action.nonce],
    };
  }
  if (action.kind === "elicitation_resolved_locally") {
    // Optimistically drop the elicitation card once the server accepts the
    // resolution (204) or reports the nonce gone (404), instead of waiting
    // on the ElicitationResolved broadcast, which the seq dedupe can drop.
    const card = state.pendingElicitations.find((e) => e.nonce === action.nonce);
    const pendingElicitations = state.pendingElicitations.filter((e) => e.nonce !== action.nonce);
    const removed = pendingElicitations.length !== state.pendingElicitations.length;
    // Record the picked answer as an optimistic overlay row (deduped by id),
    // so it shows instantly; the authoritative same-id row is server-owned
    // (the daemon folds ElicitationResolved into the transcript) and drops
    // this overlay once it lands. See #2209.
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
  if (action.kind === "hydrate") {
    return action.state;
  }
  if (action.kind === "user_prompt") {
    // Optimistic overlay row: rendered on top of the server-owned transcript
    // for instant feedback. Its id is the client-minted `prompt_id` sent on
    // the POST, so the authoritative same-id `user_prompt` row the server
    // echoes reconciles it (the overlay is dropped once that row lands in
    // `activity`). Never appended to `activity` (that is server-owned).
    //
    // Record the minted id as in flight so the spinner shows immediately;
    // the server `UserPromptSent` echo settles it by the same id, and every
    // POST failure path settles it too. See #3417 / #3173.
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
      // A fresh prompt clears stale errors: the user has indicated they're
      // trying again, so don't keep nagging them.
      startupError: null,
      lastError: null,
      inflightPromptIds: state.inflightPromptIds.includes(id)
        ? state.inflightPromptIds
        : state.inflightPromptIds.concat(id),
      promptSeq: state.promptSeq + 1,
      turnActive: true,
    };
  }
  if (action.kind === "prompt_send_rejected") {
    // The prompt POST was rejected with a 4xx (for example unsupported
    // attachments), so no `UserPromptSent` will ever acknowledge this id.
    // Settle it so Stop unlocks. The overlay row deliberately stays so the
    // user still sees what they tried to send.
    return settleInflightPrompt({ ...state, inFlightTool: null }, action.id);
  }
  if (action.kind === "settle_inflight_prompt") {
    // Every other POST outcome that no `UserPromptSent` will follow: a 5xx,
    // a network exception, a daemon `queued` disposition. Without this the
    // optimistic id latches the spinner forever, which is the same defect
    // class as #3417 arriving by a different route.
    return settleInflightPrompt(state, action.id);
  }
  if (action.kind === "rollback_optimistic_prompt") {
    // A transient send failure (worker_not_ready 503 while resuming) re-queues
    // the prompt, so its optimistic overlay row must be removed: otherwise the
    // drain's re-send would echo a second copy the server `UserPromptSent`
    // cannot reconcile. Remove exactly the overlay row we added; the prompt now
    // lives only in `queuedPrompts` (the QUEUED strip), which is server-owned
    // and drains on its own. Settle the id with it: a queued prompt is not a
    // running turn, and leaving it in flight would hold Stop over an idle
    // session. See #3094 / #3087.
    const idx = state.optimisticRows.findIndex((r) => r.id === action.id);
    if (idx === -1) return settleInflightPrompt(state, action.id);
    return settleInflightPrompt(
      {
        ...state,
        optimisticRows: state.optimisticRows.slice(0, idx).concat(state.optimisticRows.slice(idx + 1)),
      },
      action.id,
    );
  }
  if (action.kind === "enqueue_prompt") {
    // Optimistic row: shown immediately, marked `pending` until the server
    // enqueue POST is confirmed. The id is the stable key the server row
    // reconciles against (a re-POST of the same id updates in place).
    const entry: QueuedPrompt = {
      id: action.id,
      text: action.text,
      queuedAt: new Date().toISOString(),
      pending: true,
      ...(action.attachments && action.attachments.length > 0 ? { attachments: action.attachments } : {}),
    };
    return { ...state, queuedPrompts: state.queuedPrompts.concat(entry) };
  }
  if (action.kind === "dequeue_prompt") {
    return {
      ...state,
      queuedPrompts: state.queuedPrompts.filter((q) => q.id !== action.id),
    };
  }
  if (action.kind === "edit_queued_prompt") {
    return {
      ...state,
      queuedPrompts: state.queuedPrompts.map((q) => (q.id === action.id ? { ...q, text: action.text } : q)),
    };
  }
  if (action.kind === "clear_queue") {
    return { ...state, queuedPrompts: [] };
  }
  if (action.kind === "hydrate_server_queue") {
    // Reconcile the local optimistic queue with the server's authoritative
    // snapshot (ordered by seq). For a row we queued on this client, keep the
    // in-memory attachment bytes so the strip can still render a thumbnail;
    // for a row we've never seen (reload / another device) build a
    // metadata-only attachment view from the server refs (bytes stay
    // server-side and are delivered on drain).
    const rows = Array.isArray(action.rows) ? action.rows : [];
    const serverIds = new Set(rows.map((r) => r.id));
    const localById = new Map(state.queuedPrompts.map((q) => [q.id, q]));
    const merged: QueuedPrompt[] = rows.map((r) => {
      const local = localById.get(r.id);
      const attachments: PromptAttachmentInput[] | undefined = local?.attachments?.length
        ? local.attachments
        : r.attachments && r.attachments.length > 0
          ? r.attachments.map((a) => ({
              kind: a.kind,
              mimeType: a.mime_type,
              name: a.name ?? undefined,
              dataB64: "",
            }))
          : undefined;
      return {
        id: r.id,
        text: r.text,
        queuedAt: r.created_at || local?.queuedAt || new Date().toISOString(),
        ...(attachments ? { attachments } : {}),
      };
    });
    // Keep optimistic rows whose enqueue POST is still in flight (not yet in
    // the server snapshot) so a hydrate racing the POST doesn't drop them.
    const stillPending = state.queuedPrompts.filter((q) => q.pending && !serverIds.has(q.id));
    return { ...state, queuedPrompts: merged.concat(stillPending) };
  }
  if (action.kind === "confirm_queued_prompt") {
    return {
      ...state,
      queuedPrompts: state.queuedPrompts.map((q) => (q.id === action.id ? { ...q, pending: false } : q)),
    };
  }
  if (action.kind === "dismiss_primer") {
    // Clear the offer entirely so it doesn't re-render on session
    // re-mount. A subsequent SessionContextReset re-seeds the field
    // with a new `resetSeq`, which the banner reads as a fresh
    // incident and shows again. See #1110.
    return { ...state, contextPrimerAvailable: null };
  }
  if (action.kind === "dismiss_compaction_reminder") {
    // Latch the snapshot the user dismissed at. The `UsageUpdated` arm
    // re-arms on the first snapshot after any context boundary, so this
    // stays set only for the life of the current context. See #3253.
    return { ...state, compactionReminderDismissed: state.sessionUsage };
  }
  if (action.kind === "dismiss_rejected_prompt") {
    return {
      ...state,
      rejectedPrompts: state.rejectedPrompts.filter((r) => r.id !== action.id),
    };
  }
  if (action.kind === "dismiss_mode_switch_failed") {
    return { ...state, modeSwitchFailed: null };
  }
  if (action.kind === "set_pending_config_option") {
    return {
      ...state,
      pendingConfigOption: { configId: action.configId, value: action.value },
    };
  }
  if (action.kind === "clear_pending_config_option") {
    return { ...state, pendingConfigOption: null };
  }
  if (action.kind === "clear_pending_config_option_if_match") {
    if (state.pendingConfigOption?.configId === action.configId && state.pendingConfigOption?.value === action.value) {
      return { ...state, pendingConfigOption: null };
    }
    return state;
  }
  if (action.kind === "dismiss_config_option_switch_failed") {
    return { ...state, configOptionSwitchFailed: null };
  }
  return emptyAcpState();
}

/** Translate a wire {@link TranscriptDelta} (externally tagged) into the
 *  matching reducer action, mapping the carried row to the client
 *  {@link ActivityRow} shape. Returns null for an unrecognized shape. */
export function transcriptDeltaAction(delta: TranscriptDelta, sessionId: string): Action | null {
  if ("Append" in delta) {
    if (!webRendersServerRow(delta.Append)) return null;
    return { kind: "transcript_append", row: transcriptRowToActivity(delta.Append, sessionId) };
  }
  if ("Patch" in delta) {
    if (!webRendersServerRow(delta.Patch.row)) return null;
    return { kind: "transcript_patch", row: transcriptRowToActivity(delta.Patch.row, sessionId) };
  }
  if ("Remove" in delta) {
    return { kind: "transcript_remove", id: delta.Remove };
  }
  return null;
}

export type ConnectionStatus = "connecting" | "open" | "closed" | "error";

/** Reconnect backoff: 1s, 2s, 4s, 8s, 16s, 30s, 30s (cap). Seven
 *  attempts cover the common mobile-background / Cloudflare-idle /
 *  WiFi-flap recovery shapes without flooding the daemon when the
 *  backend is genuinely down. After the cap, the UI surfaces a manual
 *  "Tap to retry" affordance via `manualReconnect`. Mirrors the
 *  retry envelope already used by `useTerminal` (#1009 / #1107). */
const ACP_MAX_RETRIES = 7;
const ACP_RETRY_BASE_MS = 1000;
const ACP_RETRY_CAP_MS = 30000;
export function acpRetryDelayMs(attempt: number): number {
  return Math.min(ACP_RETRY_CAP_MS, ACP_RETRY_BASE_MS * 2 ** Math.max(0, attempt - 1));
}
export const ACP_MAX_RETRIES_EXPORT = ACP_MAX_RETRIES;

/** Liveness watchdog (#2287). The server emits a `{"kind":"heartbeat"}`
 *  Text frame every 30s. A proxy can RST the idle connection so the
 *  daemon's Close never reaches the browser, leaving the socket in
 *  `readyState === OPEN` (a zombie) while no frames arrive. Browser JS
 *  cannot see WS Ping/Pong, so the heartbeat is the only liveness
 *  signal: if none arrives within `ACP_WS_STALE_MS`, the socket is
 *  treated as dead and re-dialed. 75s tolerates two missed heartbeats
 *  plus a watchdog interval and stays under the daemon's 90s pong
 *  reaper, so the client heals first. A false positive is cheap: the
 *  redial resumes from `?since=<lastSeq>` and dedupes. */
const ACP_WS_WATCHDOG_INTERVAL_MS = 15000;
export const ACP_WS_STALE_MS = 75000;

/** A real UUID v4 for the optimistic prompt id (#3173). `crypto.randomUUID`
 *  is restricted to secure contexts (HTTPS or localhost); `aoe serve
 *  --host <LAN-IP>` is plain HTTP on a non-loopback host, where it is
 *  undefined. `crypto.getRandomValues` has no such restriction, so build
 *  the UUID from that instead of falling back to a non-UUID string. Only
 *  when crypto itself is entirely unavailable (older webviews, jsdom) does
 *  this fall back to a Date.now/Math.random id, same as StructuredWidgets.tsx's
 *  `newItemId()`. */
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
  /** Live structured view worker lifecycle from `SessionResponse.acp_worker_state`.
   *  When not `"running"`, the drain effect parks queued prompts so they
   *  don't dispatch into a worker that isn't online yet. Defaults to
   *  `"running"` so non-structured view / pre-#1088 call sites keep working. */
  workerState: "absent" | "resuming" | "running" = "running",
  /** RFC3339 archived-at, or null. `sendPrompt` clears this server-side
   *  (via PATCH /api/sessions/{id}/archive) before enqueueing so the
   *  reconciler stops skipping the session and respawns the worker.
   *  See #1581. */
  archivedAt: string | null = null,
  /** RFC3339 snoozed-until, or null. Same wake purpose as
   *  `archivedAt`, via PATCH /api/sessions/{id}/snooze with
   *  `{ minutes: null }`. See #1581. */
  snoozedUntil: string | null = null,
) {
  // Sweep stale persisted state entries on first hook mount in this
  // module's lifetime. Idempotent (guarded by `sweptStorage`) so the
  // cost is one full localStorage scan per page load.
  sweepExpiredStorage();
  const [state, dispatch] = useReducer(reducer, sessionId, initialState);
  const [status, setStatus] = useState<ConnectionStatus>("connecting");
  // Mirror the triage timestamps onto refs so `sendPrompt`'s wake
  // step always sees the freshest value without forcing a re-create
  // of the callback (the dep churn would also blow `dispatchPromptNow`
  // away on every poll). See #1581.
  const archivedAtRef = useRef(archivedAt);
  const snoozedUntilRef = useRef(snoozedUntil);
  useEffect(() => {
    archivedAtRef.current = archivedAt;
  }, [archivedAt]);
  useEffect(() => {
    snoozedUntilRef.current = snoozedUntil;
  }, [snoozedUntil]);
  // The `/clear`-boundary batching that used to happen here (slicing the
  // queued-prompt snapshot so `/clear` fires as its own turn) is now server-
  // side, in the queue drain (`queue_drain_batch`). See #1356 and the
  // server-side prompt queue design. The activity buffer is server-owned
  // (Tier 4) and the daemon enforces the `acp.replay_events` retention cap,
  // so there is no longer a client-side row cap to mirror (#1111).
  //
  // Mirror status into a ref so the WS lifecycle can read the latest value
  // without re-creating callbacks on every status flip (which would
  // invalidate downstream memoised handlers).
  const statusRef = useRef<ConnectionStatus>("connecting");
  useEffect(() => {
    statusRef.current = status;
  }, [status]);

  // Mirror every state change into the module-level cache so that on
  // remount (e.g. user navigates back to the structured view tab) we hydrate
  // from the last-known state instead of staring at an empty chat
  // until the WS connection completes.
  // Use a ref so the effect doesn't depend on sessionId directly,
  // satisfying react-you-might-not-need-an-effect/no-event-handler.
  const sessionIdRef = useRef(sessionId);
  sessionIdRef.current = sessionId;
  useEffect(() => {
    if (sessionIdRef.current) cacheSet(sessionIdRef.current, state);
  }, [state]);
  const wsRef = useRef<WebSocket | null>(null);
  // Auto-reconnect machinery (#1130). retryCountRef is the persistent
  // attempt counter across `onclose` -> scheduled `connect()` cycles;
  // retryTimerRef holds the pending setTimeout so manualReconnect can
  // cancel a backed-off retry without leaking it. countdownTimerRef
  // drives the per-second `retryCountdown` decrement that the banner
  // renders. connectRef is the stable indirection so listeners
  // installed outside the connection effect (visibilitychange, online,
  // pageshow) can dial without re-creating the listeners.
  //
  // dialGenRef is a monotonic generation counter. Every connect() call
  // bumps it; each in-flight IIFE captures its generation at entry and
  // bails (or no-ops in its WS handlers) once the current generation
  // moves past it. Without this, a visibilitychange / manualReconnect
  // that fires while a prior IIFE is mid-`await fetchReplay` allocates
  // a second WS, and the orphaned first WS's onclose still nulls
  // wsRef.current and schedules a retry on top of a healthy socket.
  const retryCountRef = useRef(0);
  const retryTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const countdownTimerRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const connectRef = useRef<(() => void) | null>(null);
  const dialGenRef = useRef(0);
  const [reconnecting, setReconnecting] = useState(false);
  const [retryCount, setRetryCount] = useState(0);
  const [retryCountdown, setRetryCountdown] = useState(0);
  // Track lastSeq in a ref so the snapshot fetcher always sees the
  // latest value without re-running the effect when it changes.
  // The ref is updated inside an effect (not during render) to keep
  // the react-hooks linter happy; fetchReplay only ever runs from
  // an event handler or another effect, so the one-tick lag is fine.
  const lastSeqRef = useRef(0);
  useEffect(() => {
    lastSeqRef.current = state.lastSeq;
  }, [state.lastSeq]);
  // Mirror the queue so the one-time server-migration read (below) sees the
  // current rows without a stale closure or a queue-sized dep array.
  const queuedPromptsRef = useRef(state.queuedPrompts);
  useEffect(() => {
    queuedPromptsRef.current = state.queuedPrompts;
  }, [state.queuedPrompts]);
  // Older-history paging (#2236). `oldestSeqRef` mirrors the recent-first
  // load watermark so `loadOlder` (a stable callback) reads it without
  // re-creating. `hasMoreOlder` / `loadingOlder` drive the scroll-up
  // affordance and guard against concurrent fetches.
  const oldestSeqRef = useRef(0);
  useEffect(() => {
    oldestSeqRef.current = state.oldestSeq;
  }, [state.oldestSeq]);
  const [hasMoreOlder, setHasMoreOlder] = useState(false);
  const hasMoreOlderRef = useRef(false);
  useEffect(() => {
    hasMoreOlderRef.current = hasMoreOlder;
  }, [hasMoreOlder]);
  const [loadingOlder, setLoadingOlder] = useState(false);
  const loadingOlderRef = useRef(false);
  // Flips true the first time the WS opens for this session and
  // resets on session change. Lets the SystemNotices banner copy
  // distinguish "first connect, worker still spawning" from
  // "reconnecting after a real drop". The prior wording was misleading
  // on brand-new sessions; see #1106.
  // Derive hasEverOpened reset from sessionId changes during render
  // rather than in a useEffect, to satisfy
  // react-you-might-not-need-an-effect/no-adjust-state-on-prop-change.
  const [hasEverOpened, setHasEverOpened] = useState(false);
  const prevSessionId1Ref = useRef(sessionId);
  if (sessionId !== prevSessionId1Ref.current) {
    prevSessionId1Ref.current = sessionId;
    setHasEverOpened(false);
  }

  // Ref-indirected setState helpers so the session effect can call them
  // without the plugin seeing direct setState calls inside an effect that
  // also subscribes to external stores. Refs are opaque to static analysis.
  const setStatusRef = useRef(setStatus);
  setStatusRef.current = setStatus;
  const setReconnectingRef = useRef(setReconnecting);
  setReconnectingRef.current = setReconnecting;
  const setRetryCountRef = useRef(setRetryCount);
  setRetryCountRef.current = setRetryCount;
  const setRetryCountdownRef = useRef(setRetryCountdown);
  setRetryCountdownRef.current = setRetryCountdown;
  const setHasEverOpenedRef = useRef(setHasEverOpened);
  setHasEverOpenedRef.current = setHasEverOpened;

  // clearRetryTimers is shared across the session effect (scheduleReconnect,
  // connect cleanup) and the auto-reconnect trigger effect below. Defined
  // as a useCallback (not inlined in any effect) so the reactive listeners
  // can reference it without re-subscribing.
  const clearRetryTimers = useCallback(() => {
    if (retryTimerRef.current) {
      clearTimeout(retryTimerRef.current);
      retryTimerRef.current = null;
    }
    if (countdownTimerRef.current) {
      clearInterval(countdownTimerRef.current);
      countdownTimerRef.current = null;
    }
  }, []);

  // Timestamp (ms) of the most recent message received on the current
  // socket (any frame, including the server heartbeat). The liveness
  // watchdog and the visibility/online triggers compare it against
  // ACP_WS_STALE_MS to spot a zombie socket. See #2287.
  const lastServerMsgRef = useRef<number>(0);

  // Stable callback ref for visibility/online/pageshow triggers so the
  // reactive effects below can reconnect without depending on sessionId
  // (satisfies react-you-might-not-need-an-effect/no-event-handler).
  const tryAutoReconnectRef = useRef<() => void>(() => {});
  tryAutoReconnectRef.current = () => {
    const ws = wsRef.current;
    const ready = ws?.readyState;
    if (ready === WebSocket.CONNECTING) return;
    // A socket that reports OPEN but has gone quiet past the stale
    // window is a half-open zombie (proxy RST the browser never saw).
    // Fall through and re-dial; connect() closes it and bumps the dial
    // generation, so we don't rely on the dead socket ever firing
    // onclose. A genuinely fresh OPEN socket bails here. See #2287.
    if (ready === WebSocket.OPEN && Date.now() - lastServerMsgRef.current < ACP_WS_STALE_MS) {
      return;
    }
    retryCountRef.current = 0;
    setRetryCount(0);
    setRetryCountdown(0);
    clearRetryTimers();
    connectRef.current?.();
  };

  // Liveness watchdog: a foreground-visible idle tab fires neither
  // visibilitychange nor online, so a zombie socket would otherwise sit
  // forever. Poll while the socket reports OPEN and let
  // tryAutoReconnect re-dial only when it's also stale. Gated on OPEN so
  // it never resurrects an intentionally-closed or retry-exhausted
  // socket; backoff owns those. See #2287.
  useEffect(() => {
    const id = setInterval(() => {
      if (wsRef.current?.readyState === WebSocket.OPEN) {
        tryAutoReconnectRef.current();
      }
    }, ACP_WS_WATCHDOG_INTERVAL_MS);
    return () => clearInterval(id);
  }, []);

  // Subscribe to visibility+pageshow and online via useSyncExternalStore
  // so no effect directly subscribes to an external store, satisfying
  // react-you-might-not-need-an-effect/no-external-store-subscription.
  const visCounterRef = useRef(0);
  const subscribeVisibility = useCallback((cb: () => void) => {
    const handler = () => {
      visCounterRef.current += 1;
      cb();
    };
    document.addEventListener("visibilitychange", handler);
    window.addEventListener("pageshow", handler);
    return () => {
      document.removeEventListener("visibilitychange", handler);
      window.removeEventListener("pageshow", handler);
    };
  }, []);
  const getVisibilitySnapshot = useCallback(() => visCounterRef.current, []);
  const visCounter = useSyncExternalStore(subscribeVisibility, getVisibilitySnapshot, () => 0);

  const isOnline = useSyncExternalStore(
    (cb: () => void) => {
      window.addEventListener("online", cb);
      window.addEventListener("offline", cb);
      return () => {
        window.removeEventListener("online", cb);
        window.removeEventListener("offline", cb);
      };
    },
    () => navigator.onLine,
    () => true, // SSR guard
  );

  // React to visibility/pageshow events: reconnect when page is restored
  // from background or bfcache. Skip the initial mount to avoid a
  // redundant connect() alongside the session effect's connect().
  const isFirstVis = useRef(true);
  useEffect(() => {
    if (isFirstVis.current) {
      isFirstVis.current = false;
      return;
    }
    tryAutoReconnectRef.current();
  }, [visCounter]);

  // React to online transitions: reconnect when the OS rejoins the
  // network (Cloudflare kills tunnel WSs after ~100s of offline).
  const prevOnlineRef = useRef(isOnline);
  useEffect(() => {
    if (!prevOnlineRef.current && isOnline) {
      tryAutoReconnectRef.current();
    }
    prevOnlineRef.current = isOnline;
  }, [isOnline]);

  // Timestamp (ms) of the most recent applied frame. Read by the
  // "Force end turn" escape hatch in WorkingSpinner: when `turnActive`
  // is true and `Date.now() - lastActivity` exceeds the configured
  // threshold, the spinner offers the button. Kept as a ref (not
  // reducer state) so updating it on every frame doesn't trigger a
  // rerender; the spinner polls the ref on its own 1s timer. See
  // #1100 (C).
  // Initialised to 0; bumped to a real timestamp on first applied
  // frame or first user submit. Date.now() at render time would trip
  // react-hooks/purity (renders must be deterministic), and the zero
  // sentinel does the right thing on first read since
  // `Date.now() - 0` is enormous and the spinner only checks against
  // it while `turnActive` is true (false on a freshly-mounted hook).
  const lastActivityRef = useRef<number>(0);

  const fetchReplay = useCallback(async (sid: string) => {
    try {
      // Cold open (cache miss, nothing loaded): recent-first. Render the
      // most recent page immediately and page older history lazily on
      // scroll-up, instead of forward-folding the whole transcript from
      // seq 0 before first paint. The warm path below (hydrated from
      // cache, lastSeq > 0) keeps the cheap forward seq-delta top-up.
      // See #2236.
      if (lastSeqRef.current === 0) {
        // The transcript (activity) and the bulk of control state are
        // server-owned, but the frames leg still feeds what the daemon does
        // not model (worker latches, monitor and wakeup badges, the usage cost
        // baseline, rejected prompts, the optimistic turn counters).
        // `?view=rows` returns the folded rows with an EMPTY `frames`, so pull
        // both projections of the SAME page in parallel: default frames feed
        // the control reducer, `view=rows` feeds the transcript. Identical
        // pagination metadata, so the frames response drives the cursors.
        const tailParams = `before=${TAIL_BEFORE}&limit=${REPLAY_PAGE_SIZE}`;
        const [tailRes, tailRowsRes] = await Promise.all([
          fetch(`/api/sessions/${encodeURIComponent(sid)}/acp/replay?${tailParams}`, { credentials: "same-origin" }),
          fetch(`/api/sessions/${encodeURIComponent(sid)}/acp/replay?${tailParams}&view=rows`, {
            credentials: "same-origin",
          }),
        ]);
        if (!tailRes.ok) return;
        const tail = (await tailRes.json()) as ReplayPageResponse;
        if (tail.lost) {
          dispatch({ kind: "lagged", skipped: tail.highest_seq });
          return;
        }
        // Bail rather than render a hole: the frames leg below advances
        // `lastSeqRef`, and the WS then drains from that cursor, so a page of
        // rows dropped here would never be resent. Returning leaves the
        // cursors untouched so the next hydrate retries the same page.
        if (!tailRowsRes.ok) return;
        const tailRows = ((await tailRowsRes.json()) as ReplayPageResponse).rows ?? [];
        dispatch({
          kind: "frames",
          frames: tail.frames ?? [],
          rows: tailRows.filter(webRendersServerRow).map((r) => transcriptRowToActivity(r, sid)),
          oldestSeq: tail.next_cursor ?? 0,
        });
        setHasMoreOlder(tail.has_more ?? false);
        // Advance the seq ref synchronously (the [state.lastSeq] effect
        // mirror lags a render tick) so the WS dial that follows this
        // awaited call subscribes with `since = highest_seq` and the
        // server drains only live events, not the whole transcript we
        // just rendered recent-first. See #2236.
        if (tail.highest_seq > lastSeqRef.current) {
          lastSeqRef.current = tail.highest_seq;
        }
        // Long session: the tail skipped the seq-0 handshake (prompt
        // capabilities, slash palette, agent/model/mode), pinned near the
        // start by #1049. Pull a small prefix and project just those
        // fields so the composer isn't crippled until the user scrolls up.
        if ((tail.has_more ?? false) && (tail.next_cursor ?? 0) > 1) {
          const hsRes = await fetch(
            `/api/sessions/${encodeURIComponent(sid)}/acp/replay?since=0&limit=${HANDSHAKE_PREFIX_SIZE}`,
            { credentials: "same-origin" },
          );
          if (hsRes.ok) {
            const hs = (await hsRes.json()) as ReplayPageResponse;
            if ((hs.frames ?? []).length > 0) dispatch({ kind: "handshake", frames: hs.frames });
          }
        }
        dispatch({ kind: "lagged_resolved" });
        return;
      }
      // Defensive overlap: re-fetch from `lastSeq - REPLAY_OVERLAP`
      // instead of `lastSeq` so events that landed in the broadcast
      // tail without being applied (WS-vs-replay race, broadcast lag
      // window, etc.) get a second chance. The reducer's
      // `frame.seq <= state.lastSeq` dedupe drops the overlap, so
      // this is idempotent. See #1100.
      const firstSince = Math.max(0, lastSeqRef.current - REPLAY_OVERLAP);
      let cursor = firstSince;
      // Snapshot the highest seq seen on the first page and stop there:
      // events appended after replay began arrive over the live WS and
      // are deduped, so chasing them here would never converge on a
      // busy session. Captured from page one's `highest_seq`.
      let target: number | null = null;
      for (;;) {
        // Same two-projection fetch as the cold tail: default frames for the
        // control reducer, `view=rows` for the server-owned transcript.
        const pageParams = `since=${cursor}&limit=${REPLAY_PAGE_SIZE}`;
        const [res, rowsRes] = await Promise.all([
          fetch(`/api/sessions/${encodeURIComponent(sid)}/acp/replay?${pageParams}`, { credentials: "same-origin" }),
          fetch(`/api/sessions/${encodeURIComponent(sid)}/acp/replay?${pageParams}&view=rows`, {
            credentials: "same-origin",
          }),
        ]);
        // Both legs page the same window, so a failure on either one has to
        // stop the loop: advancing the cursor past a page whose rows never
        // arrived leaves a hole nothing refetches.
        if (!res.ok || !rowsRes.ok) return;
        const data = (await res.json()) as ReplayPageResponse;
        const pageRows = ((await rowsRes.json()) as ReplayPageResponse).rows ?? [];
        if (target === null) {
          target = data.highest_seq;
          // Detect a server-side seq reset: the supervisor's per-session
          // counter has been forgotten (acp_disable → acp_enable,
          // or session delete+recreate with the same id), so the new
          // conversation is starting fresh from seq=1. Without this reset
          // the client-side dedupe would drop the new events because
          // `frame.seq <= state.lastSeq` is true. Only meaningful on the
          // first page, where `cursor` is the client's resume point.
          if (data.highest_seq < firstSince) {
            dispatch({ kind: "reset" });
          }
        }
        // Honor `lost` on every page: a retention prune between pages
        // can open a real gap after page one, so surface it via the
        // existing `lagged` flag and let the user reload for the full
        // transcript. Stop the loop; a partial transcript is wrong.
        if (data.lost) {
          dispatch({ kind: "lagged", skipped: data.highest_seq });
          return;
        }
        if (data.frames.length > 0 || pageRows.length > 0) {
          dispatch({
            kind: "frames",
            frames: data.frames,
            rows: pageRows.filter(webRendersServerRow).map((r) => transcriptRowToActivity(r, sid)),
          });
        }
        const next = data.next_cursor;
        if (data.has_more && next != null && next > cursor && next < target) {
          cursor = next;
          continue;
        }
        break;
      }
      dispatch({ kind: "lagged_resolved" });
    } catch {
      // Network failure: leave the lagged flag set so the user
      // sees something is wrong rather than silently dropping
      // frames.
    }
  }, []);

  // Fetch the next-older page of history and prepend it. Stable callback;
  // reads the watermark / guards from refs. The scroll-up handler and the
  // "Load earlier" button both call this once the already-loaded rows are
  // exhausted. See #2236.
  const loadOlder = useCallback(async () => {
    const sid = sessionIdRef.current;
    const before = oldestSeqRef.current;
    if (!sid || before <= 0 || loadingOlderRef.current || !hasMoreOlderRef.current) return;
    loadingOlderRef.current = true;
    setLoadingOlder(true);
    try {
      // Older history is transcript-only (the prepend never re-folds control
      // state), so a single `?view=rows` fetch suffices; no companion frames
      // request is needed here.
      const res = await fetch(
        `/api/sessions/${encodeURIComponent(sid)}/acp/replay?before=${before}&limit=${REPLAY_PAGE_SIZE}&view=rows`,
        { credentials: "same-origin" },
      );
      if (!res.ok) return;
      const data = (await res.json()) as ReplayPageResponse;
      const rows = (data.rows ?? []).filter(webRendersServerRow).map((r) => transcriptRowToActivity(r, sid));
      if (rows.length > 0) {
        dispatch({ kind: "prepend", rows, oldestSeq: data.next_cursor ?? before });
      }
      setHasMoreOlder(data.has_more ?? false);
    } catch {
      // Leave hasMoreOlder as-is; a transient failure shouldn't
      // permanently hide the affordance. The next scroll-up retries.
    } finally {
      loadingOlderRef.current = false;
      setLoadingOlder(false);
    }
  }, []);

  // Derive status and retry state from sessionId changes during render,
  // not in a useEffect, to satisfy
  // react-you-might-not-need-an-effect/no-adjust-state-on-prop-change
  // and react-hooks/set-state-in-effect.
  const prevSessionId2Ref = useRef(sessionId);
  if (sessionId !== prevSessionId2Ref.current) {
    prevSessionId2Ref.current = sessionId;
    if (!sessionId) {
      setStatus("closed");
    } else {
      setStatus("connecting");
    }
    setReconnecting(false);
    setRetryCount(0);
    setRetryCountdown(0);
    // The new session re-derives its window from its own recent-first
    // load; clear the older-paging flags so a stale "more above" doesn't
    // carry across the switch. See #2236.
    setHasMoreOlder(false);
    setLoadingOlder(false);
    loadingOlderRef.current = false;
    // Sync the seq refs to the switched-in session synchronously: the
    // [state.lastSeq] / [state.oldestSeq] effect mirrors lag a render
    // tick, so without this `fetchReplay` would read the PREVIOUS
    // session's seq and take the warm forward path instead of a
    // recent-first cold open (or fetch from the wrong cursor). Match what
    // the effect's `hydrate` restores: the cache, else 0. See #2236.
    const switched = sessionId ? cacheGet(sessionId) : undefined;
    lastSeqRef.current = switched?.lastSeq ?? 0;
    oldestSeqRef.current = switched?.oldestSeq ?? 0;
  }

  useEffect(() => {
    if (!sessionId) {
      statusRef.current = "closed";
      return;
    }
    // Hydrate the reducer from the per-session cache rather than
    // resetting to empty. fetchReplay will then top up anything that
    // happened on the server while this component was unmounted using
    // the cached lastSeq as the `since` cursor.
    dispatch({
      kind: "hydrate",
      state: cacheGet(sessionId) ?? emptyAcpState(),
    });
    statusRef.current = "connecting";
    retryCountRef.current = 0;

    // Set up cancellation so the cleanup function can stop a pending
    // open if the effect re-runs (sessionId change) before the WS dial
    // completed. Without this, a fast session-switch could leak a WS
    // that fires onmessage into a now-stale reducer.
    let cancelled = false;

    const scheduleReconnect = () => {
      if (cancelled) return;
      if (retryCountRef.current >= ACP_MAX_RETRIES) {
        setReconnectingRef.current(false);
        setRetryCountRef.current(retryCountRef.current);
        setRetryCountdownRef.current(0);
        return;
      }
      retryCountRef.current += 1;
      const attempt = retryCountRef.current;
      const delayMs = acpRetryDelayMs(attempt);
      let countdown = Math.ceil(delayMs / 1000);
      setReconnectingRef.current(true);
      setRetryCountRef.current(attempt);
      setRetryCountdownRef.current(countdown);
      clearRetryTimers();
      countdownTimerRef.current = setInterval(() => {
        countdown -= 1;
        if (countdown > 0) setRetryCountdownRef.current(countdown);
      }, 1000);
      retryTimerRef.current = setTimeout(() => {
        if (countdownTimerRef.current) {
          clearInterval(countdownTimerRef.current);
          countdownTimerRef.current = null;
        }
        connectRef.current?.();
      }, delayMs);
    };

    const connect = () => {
      if (cancelled) return;
      // Cancel any pending scheduled retry; a fresh dial supersedes it.
      clearRetryTimers();
      // Bump the dial generation BEFORE closing any prior socket, so a
      // synchronously-firing `onclose` sees itself as orphaned
      // (`isCurrentDial()` false) and bails instead of re-arming
      // scheduleReconnect on top of this fresh dial. This matters when
      // connect() is invoked on a still-OPEN zombie socket (the
      // staleness watchdog path, #2287); the previous order only worked
      // because every other caller reached connect() with an
      // already-closed or null socket.
      dialGenRef.current += 1;
      if (wsRef.current) {
        try {
          wsRef.current.close();
        } catch {
          // ignore
        }
        wsRef.current = null;
      }
      const myGen = dialGenRef.current;
      const isCurrentDial = () => !cancelled && dialGenRef.current === myGen;
      statusRef.current = "connecting";
      void (async () => {
        // Order: replay first, then open WS. Today the server's WS
        // on-connect drain and the REST replay endpoint read the same
        // disk store; awaiting the replay before the dial gives the
        // reducer a known-correct `lastSeq` so the WS subscribes from a
        // settled cursor instead of racing two delivery paths. Without
        // this, an event landing during the dial window could be
        // delivered by both paths in different orders, and the dedupe
        // would drop later applies, which is exactly the "Stopped never
        // reaches the reducer" failure mode in #1100.
        await fetchReplay(sessionId);
        if (!isCurrentDial()) return;

        const token = getToken();
        const protocol = window.location.protocol === "https:" ? "wss" : "ws";
        // Pass `?since=<lastSeq>` so the server's on-connect drain only
        // resends events newer than what we already have. Without this,
        // a long-running session resends its full transcript on every
        // reconnect (page refresh / mobile flap), which can be tens of
        // MB at the retention cap.
        const since = lastSeqRef.current;
        const url = `${protocol}://${window.location.host}/sessions/${encodeURIComponent(sessionId)}/acp/ws?since=${since}`;

        // Subprotocols carry both factors on a WS upgrade:
        //   - `aoe-auth` is the legacy signalling protocol the server
        //     expects to see.
        //   - the bare `<token>` is the first-factor auth token
        //     (kept for backward compatibility with PWA tabs that
        //     loaded before the prefixed format landed).
        //   - `aoe-device.<binding-secret>` is the device-binding
        //     second factor introduced in #1131. The middleware
        //     enforces this when passphrase login is configured.
        let bindingSecret: string | null = null;
        try {
          bindingSecret = getOrCreateDeviceBindingSecret();
        } catch {
          // Storage / crypto unavailable; the server will reject this
          // upgrade with 401 and the login page will surface the cause.
        }
        const protocols: string[] = ["aoe-auth"];
        if (token) protocols.push(token);
        if (bindingSecret) protocols.push(`aoe-device.${bindingSecret}`);
        const ws = new WebSocket(url, protocols);
        wsRef.current = ws;

        // Set the ref synchronously alongside setState so sendPrompt's
        // gate (which reads the ref) doesn't race the next render.
        // Without this, a click landing in the same event-loop tick as
        // `onclose` could see statusRef.current === "open" and dispatch
        // an optimistic prompt against a closed socket.
        //
        // Every handler additionally checks `isCurrentDial()`: an
        // orphaned WS from a superseded connect() must not flip status,
        // null wsRef.current, or schedule a retry on top of the new
        // healthy socket.
        ws.onopen = () => {
          if (!isCurrentDial()) {
            try {
              ws.close();
            } catch {
              // ignore
            }
            return;
          }
          statusRef.current = "open";
          setStatusRef.current("open");
          setHasEverOpenedRef.current(true);
          // Seed the liveness clock so a slow first heartbeat doesn't
          // trip the staleness watchdog right after connect. See #2287.
          lastServerMsgRef.current = Date.now();
          // A live socket is the right moment to reset the retry
          // envelope: a future close from here is a genuinely new
          // failure, not a continuation of the prior backoff chain.
          retryCountRef.current = 0;
          setReconnectingRef.current(false);
          setRetryCountRef.current(0);
          setRetryCountdownRef.current(0);
        };
        ws.onerror = () => {
          if (!isCurrentDial()) return;
          statusRef.current = "error";
          setStatusRef.current("error");
        };
        ws.onclose = () => {
          if (!isCurrentDial()) return;
          statusRef.current = "closed";
          setStatusRef.current("closed");
          wsRef.current = null;
          scheduleReconnect();
        };
        ws.onmessage = (ev) => {
          if (!isCurrentDial()) return;
          // Any message on the current socket proves the browser-visible
          // path is alive, so refresh the liveness clock before parsing.
          // The server heartbeat keeps this fresh on quiet sessions; a
          // busy stream keeps it fresh too. See #2287.
          lastServerMsgRef.current = Date.now();
          try {
            const data = JSON.parse(ev.data) as
              | AcpFrame
              | { kind: "lagged"; skipped?: number }
              | { kind: "heartbeat" }
              | { kind: "reduced_state" }
              | { kind: "transcript_snapshot"; rows?: TranscriptRow[] }
              | { kind: "transcript_delta"; delta?: TranscriptDelta };
            const kind = typeof data === "object" && data !== null ? (data as { kind?: unknown }).kind : undefined;
            if (kind === "heartbeat") {
              // Keepalive tick; liveness clock already bumped above.
              return;
            }
            if (kind === "lagged") {
              const skipped = (data as { skipped?: number }).skipped ?? 0;
              dispatch({ kind: "lagged", skipped });
              // Try to recover via the snapshot endpoint.
              fetchReplay(sessionId);
              return;
            }
            if (kind === "reduced_state") {
              // Server-folded control state (Tier 1.2), sent on connect and
              // after every event. Authoritative: the client no longer
              // derives any of these fields.
              const reduced = (data as { state?: ReducedState }).state;
              if (reduced) {
                lastActivityRef.current = Date.now();
                const unchanged = (data as { unchanged?: string[] }).unchanged ?? [];
                dispatch({ kind: "reduced_state", state: reduced, unchanged });
              }
              return;
            }
            if (kind === "transcript_snapshot") {
              // Server-owned transcript connect snapshot (Tier 4). Usually
              // empty (the WS dials at the current lastSeq); carries gap rows
              // on a reconnect that raced live events. Merged by row id.
              const rows = ((data as { rows?: TranscriptRow[] }).rows ?? [])
                .filter(webRendersServerRow)
                .map((r) => transcriptRowToActivity(r, sessionId));
              lastActivityRef.current = Date.now();
              dispatch({ kind: "transcript_snapshot", rows });
              return;
            }
            if (kind === "transcript_delta") {
              const delta = (data as { delta?: TranscriptDelta }).delta;
              const act = delta ? transcriptDeltaAction(delta, sessionId) : null;
              if (act) {
                lastActivityRef.current = Date.now();
                dispatch(act);
              }
              return;
            }
            if (typeof data === "object" && data !== null && "session_id" in data && "event" in data) {
              // Raw event frame: feeds the client-side CONTROL reducer only
              // (the transcript is server-owned now). Every incoming live
              // frame is an "activity" tick for the force-end-turn watchdog:
              // as long as the agent is streaming, the spinner stays "honest"
              // and the escape hatch doesn't appear. See WorkingSpinner.
              lastActivityRef.current = Date.now();
              dispatch({ kind: "frame", frame: data as AcpFrame });
            }
          } catch {
            // Ignore malformed frames; the server should never send them.
          }
        };
      })();
    };
    connectRef.current = connect;
    connect();

    return () => {
      cancelled = true;
      // Bump the generation so any in-flight IIFE / pending WS handlers
      // from this effect's lifetime see themselves as stale.
      dialGenRef.current += 1;
      clearRetryTimers();
      const ws = wsRef.current;
      if (ws) {
        try {
          ws.close();
        } catch {
          // ignore
        }
      }
      wsRef.current = null;
      connectRef.current = null;
    };
  }, [sessionId, fetchReplay, clearRetryTimers]);

  const resolveApproval = useCallback(
    async (nonce: string, decision: ApprovalDecision) => {
      if (!sessionId) return;
      try {
        const res = await fetch(
          `/api/sessions/${encodeURIComponent(sessionId)}/acp/approvals/${encodeURIComponent(nonce)}`,
          {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ decision }),
          },
        );
        const detail = res.ok ? "" : await safeText(res);
        const outcome = classifyApprovalResolveResponse(res.ok, res.status, detail, nonce);
        if (outcome.kind === "resolved") {
          // 204, or a 404 that names the missing nonce: the decision was
          // accepted or already resolved server-side (a concurrent
          // decision, a watchdog cancel, or the agent picking no matching
          // option). Clear the card now rather than waiting on the
          // ApprovalResolved broadcast, which the seq dedupe can drop and
          // strand the card. A session-gone 404 (different body) is a real
          // failure and surfaces an error. See #1821.
          dispatch({ kind: "approval_resolved_locally", nonce });
        } else {
          dispatch({ kind: "error", message: outcome.message });
        }
      } catch (e) {
        dispatch({
          kind: "error",
          message: `Network error resolving approval: ${describeError(e)}`,
        });
      }
    },
    [sessionId],
  );

  const resolveElicitation = useCallback(
    async (nonce: string, resolution: ElicitationResolution) => {
      if (!sessionId) return;
      let res: Response;
      try {
        res = await fetch(
          `/api/sessions/${encodeURIComponent(sessionId)}/acp/elicitations/${encodeURIComponent(nonce)}`,
          {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify(resolution),
          },
        );
      } catch (e) {
        dispatch({
          kind: "error",
          message: `Network error resolving question: ${describeError(e)}`,
        });
        // Rethrow so the card re-enables (it can be resubmitted).
        throw e;
      }
      const detail = res.ok ? "" : await safeText(res);
      const outcome = classifyElicitationResolveResponse(res.ok, res.status, detail, nonce);
      if (outcome.kind === "resolved") {
        dispatch({ kind: "elicitation_resolved_locally", nonce, resolution });
        return;
      }
      // A validation rejection (422) leaves the elicitation pending
      // server-side, so surface the reason and rethrow: the card resets to
      // its editable state and the user can correct and resubmit the same
      // nonce instead of the question being stranded. See #2100.
      dispatch({ kind: "error", message: outcome.message });
      throw new Error(outcome.message);
    },
    [sessionId],
  );

  // POST a prompt and report what the daemon did with it. Internal helper
  // used by both sendPrompt and the drain effect below (when popping the head
  // of queuedPrompts on Stopped). The result tells the drain effect what to do
  // with the items it just sent:
  //   - "dispatched" / "queued": the daemon accepted it, retire them.
  //   - "non_retryable_failure": the server rejected them with a 4xx, so
  //     retrying would just re-POST the same failing batch every turn-end;
  //     retire them too (the error banner already surfaced the reason).
  //   - "retryable_failure": a transient disconnect / 5xx / network error,
  //     so keep the queue intact for the next turn-end retry.
  const dispatchPromptNow = useCallback(
    async (text: string, attachments?: PromptAttachmentInput[]): Promise<PromptSendResult> => {
      if (!sessionId) return { kind: "retryable_failure" };
      // Optimistic preview rows: render the attachment inline from a
      // local data URL so the bubble shows immediately, before the
      // server confirms and replay would otherwise back it with the
      // GET endpoint. See #1000 / #965.
      const previews: AcpAttachment[] = (attachments ?? []).map((a, i) => ({
        id: `local-${Date.now()}-${i}`,
        kind: a.kind,
        mimeType: a.mimeType,
        name: a.name,
        size: Math.floor((a.dataB64.length * 3) / 4),
        url: `data:${a.mimeType};base64,${a.dataB64}`,
      }));
      // Mint a stable prompt id and render an optimistic overlay row keyed by
      // it. The POST echoes this id as the `Event::UserPromptSent.prompt_id`,
      // and the server-owned transcript keys the authoritative `user_prompt`
      // row on it, so the overlay reconciles by id (dropped once the server
      // row lands) instead of the old fragile text match. If the POST fails
      // with a 4xx the overlay stays so the user sees what they tried to send;
      // a transient worker_not_ready 503 rolls this exact overlay back (by id)
      // because the prompt is re-queued and the drain would otherwise echo a
      // duplicate. See #3173 / #3094 / #3087.
      const promptId = optimisticPromptId();
      dispatch({
        kind: "user_prompt",
        id: promptId,
        text,
        attachments: previews.length > 0 ? previews : undefined,
      });
      // Submit counts as activity so the force-end-turn watchdog
      // doesn't surface the escape hatch immediately on a fresh prompt
      // (the agent's first chunk can be a few seconds out).
      lastActivityRef.current = Date.now();
      try {
        const body = {
          text,
          prompt_id: promptId,
          attachments: (attachments ?? []).map((a) => ({
            kind: a.kind,
            mime_type: a.mimeType,
            data: a.dataB64,
            name: a.name,
          })),
        };
        const res = await fetch(`/api/sessions/${encodeURIComponent(sessionId)}/acp/prompt`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(body),
        });
        if (!res.ok) {
          const detail = await safeText(res);
          // 4xx means the server rejected the prompt (validation,
          // capability gate, unknown session), so there is no in-flight
          // turn to cancel and no Stopped frame to retire our optimistic
          // turn marker.
          const rejected = res.status >= 400 && res.status < 500;
          // Typed transient: the session was idle-auto-stopped (#1689) and
          // its worker did not finish respawning within send_prompt's wait
          // window. The worker is still coming online, so this is
          // retryable; suppress the error banner (the queued indicator and
          // respawn are the right signal) and let the drain re-fire it on
          // the next AcpSessionAssigned. A capacity 503 ("worker_capacity_full")
          // is NOT this case: it needs operator action, so it keeps its
          // banner. See #1748.
          //
          // Attachments are now re-queued on this transient (the queue
          // carries them in memory; see sendPrompt + the drain effect), so
          // suppress the banner for attachment sends too. See #1833.
          const workerNotReady = res.status === 503 && detail.startsWith("worker_not_ready");
          if (rejected) {
            dispatch({ kind: "prompt_send_rejected", id: promptId });
          } else if (workerNotReady) {
            // Undo the optimistic overlay row: the caller re-queues this
            // prompt, so it must live only in the queue until the drain
            // resends it once the worker is back online. See #3094 / #3087.
            dispatch({ kind: "rollback_optimistic_prompt", id: promptId });
          } else {
            // Any other 5xx. No `UserPromptSent` is coming, so settle the
            // optimistic marker; the overlay row stays so the user can see
            // what they tried to send. See #3417.
            dispatch({ kind: "settle_inflight_prompt", id: promptId });
          }
          if (!workerNotReady) {
            dispatch({
              kind: "error",
              message: `Could not send prompt (${res.status}). ${detail}`.trim(),
            });
          }
          return { kind: rejected ? "non_retryable_failure" : "retryable_failure" };
        }
        // The daemon reports what it did (Tier 3). A `queued` disposition means
        // it parked the prompt rather than starting a turn, so the optimistic
        // transcript row has to become a queue row: the turn-end drain will
        // deliver it, and leaving the transcript row would show the message as
        // sent while it waits.
        const dispatched = (await safeJson<PromptDispatchBody>(res)) ?? {};
        if (dispatched.disposition === "queued") {
          dispatch({ kind: "rollback_optimistic_prompt", id: promptId });
          return { kind: "queued", queuedId: dispatched.queued_id ?? promptId };
        }
        return { kind: "dispatched" };
      } catch (e) {
        // The POST never completed, so nothing will acknowledge this id.
        // Settling it is the safe direction: a brief false idle converges on
        // the next control frame, a false active never converges. See #3417.
        dispatch({ kind: "settle_inflight_prompt", id: promptId });
        dispatch({
          kind: "error",
          message: `Network error sending prompt: ${describeError(e)}`,
        });
        return { kind: "retryable_failure" };
      }
    },
    [sessionId],
  );

  // Queue a prompt on the server with an optimistic local row. The row shows
  // immediately (marked `pending`); the enqueue POST persists it (and buffers
  // any attachment bytes) server-side, then the daemon drains it at turn-end
  // with no tab open. On confirm the `pending` flag clears; on failure the row
  // stays visible with an error so the message is not silently lost. Attachment
  // shapes match (`PromptAttachmentInput` == `QueueAttachmentUpload`), so they
  // pass straight through.
  const enqueueServer = useCallback(
    (text: string, attachments?: PromptAttachmentInput[]) => {
      if (!sessionId) return;
      const id = `q-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
      dispatch({ kind: "enqueue_prompt", id, text, attachments });
      // Queue depth is now server-owned, but keep the opt-in telemetry signal.
      reportAcpInteraction("prompt_queued");
      void (async () => {
        const row = await enqueueServerPrompt(sessionId, { id, text, attachments });
        if (row) {
          dispatch({ kind: "confirm_queued_prompt", id });
        } else {
          dispatch({
            kind: "error",
            message: "Couldn't queue your message on the server; it may not send. Remove it and try again.",
          });
        }
      })();
    },
    [sessionId],
  );

  // The daemon owns the send, steer, or queue decision.
  const sendPrompt = useCallback(
    async (text: string, attachments?: PromptAttachmentInput[]) => {
      if (!sessionId) return;
      // Wake archived or snoozed sessions before posting; the reconciler skips
      // them and therefore cannot drain a prompt queued while they remain sunk.
      if (archivedAtRef.current || snoozedUntilRef.current) {
        const wakeResult = archivedAtRef.current
          ? await setSessionArchive(sessionId, false)
          : await setSessionSnooze(sessionId, null);
        if (!wakeResult) {
          // Do not queue against a session the reconciler still skips.
          dispatch({
            kind: "error",
            message: "Could not wake this session. Please retry, or unarchive / unsnooze from the sidebar.",
          });
          return;
        }
      }
      const result = await dispatchPromptNow(text, attachments);
      if (result.kind === "queued") {
        // The daemon parked it. The row already exists server-side, so show it
        // in the strip as confirmed rather than POSTing a second copy.
        dispatch({ kind: "enqueue_prompt", id: result.queuedId, text, attachments });
        dispatch({ kind: "confirm_queued_prompt", id: result.queuedId });
        reportAcpInteraction("prompt_queued");
        return;
      }
      // A transient 503 means the daemon accepted the request but its worker
      // did not come online within `send_prompt`'s wait window (#1748 / #1833).
      // The prompt is not on the queue (the daemon decided to send it), so
      // enqueue it here or it is lost. Both daemon-side states that answer
      // "sent" for a session with no worker need this: the idle-dormant wake
      // and the rate-limit redelivery-cap park, whose banner tells the user a
      // fresh prompt is the recovery (#3688).
      if (result.kind === "retryable_failure" && (state.workerIdleStopped || state.rateLimitRetriesExhausted)) {
        enqueueServer(text, attachments);
      }
    },
    [sessionId, state.workerIdleStopped, state.rateLimitRetriesExhausted, dispatchPromptNow, enqueueServer],
  );

  // Server-queue hydration. The daemon owns the queue and drains it (even
  // with no tab open), so the client's job is to reflect the server snapshot,
  // not to drain. Re-list on connect and whenever a turn ends (a server drain
  // fires at turn-end, so the drained rows disappear on the following list)
  // and dispatch a reconcile that keeps this session's optimistic thumbnails
  // and any still-in-flight enqueue.
  //
  // The FIRST run also migrates: it pushes any rows queued before the server
  // owned the queue (rows restored from localStorage, or in-flight) to the
  // server, keyed by their existing id so the POST is idempotent, then lists.
  // Attachments survive migration only for rows still in memory (localStorage
  // drops the bytes), matching the prior reload behavior. See the server-side
  // prompt queue design.
  // Keyed by session id, not a bare boolean: the hook instance outlives a
  // session switch (the SPA swaps `sessionId` without remounting), so a single
  // flag meant only the first session a tab ever opened got migrated and every
  // later one silently skipped it, stranding its pre-server-queue rows in
  // localStorage.
  const queueMigratedRef = useRef<Set<string>>(new Set());
  useEffect(() => {
    if (!sessionId) return;
    if (status !== "open") return;
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
      if (cancelled) return;
      dispatch({ kind: "hydrate_server_queue", rows });
    })();
    return () => {
      cancelled = true;
    };
  }, [sessionId, status, state.turnActive]);

  // Optimistic local update + server mutation. The server is authoritative;
  // a later hydrate reconciles. A failed mutation is best-effort (the row
  // reappears on the next hydrate), so we don't roll the optimistic edit back.
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

  // Stable handle to `cancelPrompt` (defined below) so `sendQueuedNow` can
  // interrupt a running turn without a forward reference. Assigned in the
  // render body right after `cancelPrompt` is created.
  const cancelPromptRef = useRef<() => Promise<void> | void>(() => {});

  // Send directly when possible. For a non-steerable active turn, cancel and
  // let the server drain rather than posting during cancellation.
  const sendQueuedNow = useCallback(
    async (prompt: QueuedPrompt) => {
      const sid = sessionIdRef.current;
      if (!sid) return;
      const steerable = !!state.promptCapabilities?.steering && !state.cancelling && !state.compacting;
      if (state.turnActive && !steerable) {
        await cancelPromptRef.current();
        return;
      }
      // A hydrated attachment row has only server-side bytes. Removing it would
      // delete those bytes before the direct POST, so leave it for the drain.
      if (prompt.attachments?.some((a) => !a.dataB64)) return;
      // Remove server-side first so the turn-end drain can't also deliver this
      // row, then send it directly. The optimistic dequeue hides it locally.
      // The remove takes the same per-instance lock the drain holds across its
      // send, so it cannot interleave with a drain that already snapshotted
      // this row.
      dispatch({ kind: "dequeue_prompt", id: prompt.id });
      const removed = await removeServerQueuedPrompt(sid, prompt.id);
      if (!removed) {
        // The row was already gone, which means the drain claimed it while we
        // were asking. It is being delivered; sending again would double it.
        return;
      }
      const result = await dispatchPromptNow(prompt.text, prompt.attachments);
      if (result.kind === "queued") {
        // The daemon parked it again (the turn it would jump ahead of is still
        // running). Put the row back so the strip keeps showing it.
        dispatch({ kind: "enqueue_prompt", id: result.queuedId, text: prompt.text, attachments: prompt.attachments });
        dispatch({ kind: "confirm_queued_prompt", id: result.queuedId });
      } else if (result.kind === "retryable_failure") {
        // The immediate send bounced (worker still resuming); re-queue it
        // server-side so the drain re-fires it, and restore the optimistic row.
        enqueueServer(prompt.text, prompt.attachments);
      }
    },
    [
      dispatchPromptNow,
      enqueueServer,
      state.turnActive,
      state.promptCapabilities?.steering,
      state.cancelling,
      state.compacting,
    ],
  );

  const dismissPrimer = useCallback(() => {
    dispatch({ kind: "dismiss_primer" });
  }, []);

  const dismissCompactionReminder = useCallback(() => {
    dispatch({ kind: "dismiss_compaction_reminder" });
  }, []);

  const dismissRejectedPrompt = useCallback((id: string) => {
    dispatch({ kind: "dismiss_rejected_prompt", id });
  }, []);

  const dismissModeSwitchFailed = useCallback(() => {
    dispatch({ kind: "dismiss_mode_switch_failed" });
  }, []);

  // Send `session/set_config_option` to the daemon (model / reasoning
  // effort / future selector). Pessimistic: the current value stays put
  // until the adapter pushes a confirming `ConfigOptionsUpdated`. The
  // pending dispatch records the in-flight click so the UI can dim the
  // just-clicked option without lying about active state. On HTTP
  // failure the pending state clears and lastError surfaces a banner;
  // adapter-side rejection comes back as a `ConfigOptionSwitchFailed`
  // frame which clears pending in the reducer and renders a
  // non-blocking notice. See #1403.
  const setConfigOption = useCallback(
    async (configId: string, value: string) => {
      if (!sessionId) return;
      dispatch({ kind: "set_pending_config_option", configId, value });
      try {
        const res = await fetch(`/api/sessions/${encodeURIComponent(sessionId)}/acp/config-option`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ config_id: configId, value }),
        });
        if (!res.ok) {
          const detail = await safeText(res);
          // Guard against the user clicking a second option before this
          // request's response landed: clear pending only when it
          // still matches our (configId, value) pair. See #1403.
          dispatch({
            kind: "clear_pending_config_option_if_match",
            configId,
            value,
          });
          dispatch({
            kind: "error",
            message: `Could not set ${configId} (${res.status}). ${detail}`.trim(),
          });
        }
      } catch (e) {
        dispatch({
          kind: "clear_pending_config_option_if_match",
          configId,
          value,
        });
        dispatch({
          kind: "error",
          message: `Network error setting ${configId}: ${describeError(e)}`,
        });
      }
    },
    [sessionId],
  );

  const dismissConfigOptionSwitchFailed = useCallback(() => {
    dispatch({ kind: "dismiss_config_option_switch_failed" });
  }, []);

  // Cancels the in-flight agent turn (ACP session/cancel). Must only
  // fire on an explicit user gesture against a dedicated cancel/stop
  // affordance; never bind this to the Escape key. Claude Code CLI
  // hijacks Escape for cancel and accidental presses lose work the
  // user did not mean to abort; the structured view deliberately keeps Escape
  // for closing local UI surfaces (palette, dialogs, popovers) only.
  // If a future Escape binding is added, route it through
  // useKeyboardShortcuts.onEscape's local-UI dismissal, not here.
  const cancelPrompt = useCallback(async () => {
    if (!sessionId) return;
    try {
      const res = await fetch(`/api/sessions/${encodeURIComponent(sessionId)}/acp/cancel`, { method: "POST" });
      if (!res.ok) {
        const detail = await safeText(res);
        dispatch({
          kind: "error",
          message: `Could not cancel (${res.status}). ${detail}`.trim(),
        });
      }
    } catch (e) {
      dispatch({
        kind: "error",
        message: `Network error cancelling: ${describeError(e)}`,
      });
    }
  }, [sessionId]);
  cancelPromptRef.current = cancelPrompt;

  // Escape hatch for the "spinner stuck" failure mode (#1100). POSTs to
  // the daemon and relies on the server-published Stopped event to drive
  // reducer state: either the synthetic free-the-UI Stopped or the
  // user_forced one from the worker restart. We do NOT fabricate a
  // client-side Stopped seq; the server echo flows back as a real frame
  // on the WS. See #1727.
  const forceEndTurn = useCallback(async () => {
    if (!sessionId) return;
    lastActivityRef.current = Date.now();
    try {
      const res = await fetch(`/api/sessions/${encodeURIComponent(sessionId)}/acp/force_end_turn`, { method: "POST" });
      if (!res.ok) {
        const detail = await safeText(res);
        dispatch({
          kind: "error",
          message: `Could not force end turn (${res.status}). ${detail}`.trim(),
        });
      }
    } catch (e) {
      dispatch({
        kind: "error",
        message: `Network error forcing end turn: ${describeError(e)}`,
      });
    }
  }, [sessionId]);

  const dismissError = useCallback(() => {
    dispatch({ kind: "clear_error" });
  }, []);

  // Public manual-reconnect affordance. Surfaces in the SystemNotices
  // banner once the auto-retry envelope is exhausted; resets the
  // backoff counter and dials a fresh WS immediately. Idempotent
  // against a live socket (the reconnect path checks readyState).
  const manualReconnect = useCallback(() => {
    if (retryTimerRef.current) {
      clearTimeout(retryTimerRef.current);
      retryTimerRef.current = null;
    }
    if (countdownTimerRef.current) {
      clearInterval(countdownTimerRef.current);
      countdownTimerRef.current = null;
    }
    retryCountRef.current = 0;
    setRetryCount(0);
    setRetryCountdown(0);
    setReconnecting(false);
    connectRef.current?.();
  }, []);

  // Whether the per-row "Send now" affordance can do something useful: the
  // socket is open, no worker-down banner is up, and the worker is either
  // Active turns remain eligible because Send now may intentionally interrupt.
  const canSendQueuedNow =
    status === "open" &&
    !state.workerStopped &&
    !state.workerRestarting &&
    (workerState === "running" || state.workerIdleStopped || state.rateLimitRetriesExhausted);

  // True when pressing "Send now" would interrupt a running, non-steerable turn
  // rather than send immediately, so the affordance can warn before it cancels
  // the agent's in-flight work.
  const sendNowInterruptsTurn =
    state.turnActive && !(state.promptCapabilities?.steering && !state.cancelling && !state.compacting);

  return {
    state,
    status,
    /** True while retrying a closed socket. */
    reconnecting,
    /** Current attempt number; 0 while the live socket is healthy,
     *  1..MAX while backing off. */
    retryCount,
    /** Seconds until the next retry. */
    retryCountdown,
    /** Retry limit exposed for banner copy. */
    maxRetries: ACP_MAX_RETRIES,
    /** Reset retry state and dial immediately. */
    manualReconnect,
    /** Distinguishes initial connection from recovery. */
    hasEverOpened,
    resolveApproval,
    resolveElicitation,
    sendPrompt,
    cancelPrompt,
    forceEndTurn,
    /** Fetch and prepend the next-older page of history. No-op when a
     *  fetch is already in flight or no older events remain. See #2236. */
    loadOlder,
    /** True when older events exist on the server beyond what's loaded,
     *  so the scroll-up handler and "Load earlier" button should offer
     *  to fetch more. See #2236. */
    hasMoreOlder,
    /** True while a `loadOlder` fetch is in flight; drives a spinner on
     *  the load-earlier affordance. See #2236. */
    loadingOlder,
    /** Timestamp (ms) of the most recent applied frame. The
     *  WorkingSpinner reads this on a 1s timer to decide whether to
     *  surface the "Force end turn" button. Exposed as a ref so the
     *  hook doesn't rerender every frame just to update a watchdog
     *  clock. See #1100 (C). */
    lastActivityRef,
    dismissError,
    dismissPrimer,
    dismissCompactionReminder,
    removeQueuedPrompt,
    editQueuedPrompt,
    clearQueue,
    sendQueuedNow,
    canSendQueuedNow,
    sendNowInterruptsTurn,
    dismissRejectedPrompt,
    dismissModeSwitchFailed,
    setConfigOption,
    dismissConfigOptionSwitchFailed,
  };
}

async function safeText(res: Response): Promise<string> {
  try {
    return (await res.text()).slice(0, 200);
  } catch {
    return "";
  }
}

/** Parse a JSON body, `null` on anything unparseable. Used where a missing or
 *  malformed body has a sane default rather than being an error. */
async function safeJson<T>(res: Response): Promise<T | null> {
  try {
    return (await res.json()) as T;
  } catch {
    return null;
  }
}

function describeError(e: unknown): string {
  if (e instanceof Error) return e.message;
  return String(e);
}
