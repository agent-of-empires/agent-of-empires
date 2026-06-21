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
  emptyAcpState,
  isTurnActive,
  normaliseTurnCounters,
  reduceFrames,
  setActivityLimit,
  summarizeAnswers,
  type ApprovalDecision,
  type AcpAttachment,
  type AcpFrame,
  type AcpState,
  type ElicitationResolution,
  type PromptAttachmentInput,
  type QueuedPrompt,
} from "../lib/acpTypes";
import { useAcpPrefs } from "../lib/acpPrefs";
import { isClearAlias } from "../lib/agentProfiles";
import { useAgentProfile } from "../lib/agentProfileContext";
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
import { reportAcpInteraction, setSessionArchive, setSessionSnooze } from "../lib/api";

/** Outcome of an immediate prompt POST, used by the drain effect to
 *  decide whether to retire queued items (delivered or permanently
 *  rejected) or keep them for a later retry (transient failure). */
type PromptSendResult = "ok" | "retryable_failure" | "non_retryable_failure";

export type Action =
  | { kind: "frame"; frame: AcpFrame }
  | { kind: "frames"; frames: AcpFrame[]; oldestSeq?: number }
  | { kind: "prepend"; frames: AcpFrame[]; oldestSeq: number }
  | { kind: "handshake"; frames: AcpFrame[] }
  | { kind: "lagged"; skipped: number }
  | { kind: "user_prompt"; text: string; attachments?: AcpAttachment[] }
  | { kind: "prompt_send_rejected" }
  | { kind: "error"; message: string }
  | { kind: "clear_error" }
  | { kind: "approval_resolved_locally"; nonce: string }
  | { kind: "elicitation_resolved_locally"; nonce: string; resolution: ElicitationResolution }
  | { kind: "lagged_resolved" }
  | { kind: "reset" }
  | { kind: "hydrate"; state: AcpState }
  | {
      kind: "enqueue_prompt";
      text: string;
      attachments?: PromptAttachmentInput[];
    }
  | { kind: "dequeue_prompt"; id: string }
  | { kind: "dequeue_prompts_by_id"; ids: string[] }
  | { kind: "edit_queued_prompt"; id: string; text: string }
  | { kind: "clear_queue" }
  | { kind: "dismiss_primer" }
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

// LRU-capped module cache keyed by structured view session id. Mirrors the
// per-session AcpState into `localStorage` under
// `aoe:acp-state:v1:<id>` so a full page reload (mobile OS evicts
// the tab, user pulls down a Cloudflare re-auth, PWA cold start)
// hydrates the reducer from the last-known state and only fetches
// the seq-delta from the server instead of replaying the entire
// transcript through the typewriter. See #1132.
//
// Versioned key prefix lets us invalidate stored entries when the
// reducer schema changes meaningfully (bump to `v2:`); a TTL sweep
// on first mount prunes entries older than STATE_TTL_MS so abandoned
// sessions don't squat on the per-origin quota forever.
//
// The cap prevents long-running dashboards from accumulating state
// for every structured view session ever opened (Map.set with an existing
// key is a no-op for ordering, so we delete-then-set to refresh the
// LRU position). `clearAcpCache(id?)` is exported so the
// session-delete handler and logout flow can drop stale entries
// instead of waiting for them to age out.
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
  if (!state.queuedPrompts.some((q) => q.attachments?.length)) return state;
  return {
    ...state,
    queuedPrompts: state.queuedPrompts.filter((q) => !q.attachments?.length),
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
    // array and crashes on `.map`. Then backfill the seq-counter pair
    // introduced by #1170; see `normaliseTurnCounters` for the rules.
    const merged: AcpState = { ...emptyAcpState(), ...(state as AcpState) };
    return normaliseTurnCounters(merged);
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

/** Shape of the `acp/replay` JSON response (forward and backward modes). */
type ReplayPageResponse = {
  frames: AcpFrame[];
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

/** Build the single combined prompt fired when
 *  `acp.queue_drain_mode = combined` and the agent transitions to
 *  idle with a non-empty queue. Joins every queued entry's text with a
 *  blank line so the agent sees them as one batch follow-up. Extracted
 *  for testability; consumed by the drain effect below. See #1031. */
export function combineQueuedPrompts(queue: ReadonlyArray<QueuedPrompt>): string {
  // Skip empty entries: an attachment-only queued prompt carries no
  // text, and gluing its "" in would leave stray blank-line separators.
  return queue
    .map((q) => q.text)
    .filter((t) => t.length > 0)
    .join("\n\n");
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

export function reducer(state: AcpState, action: Action): AcpState {
  if (action.kind === "frame") {
    return applyEvent(state, action.frame);
  }
  if (action.kind === "frames") {
    const next = action.frames.reduce(applyEvent, state);
    // The recent-first tail load passes the page's lowest seq so the
    // first forward fold seeds the older-history watermark. Live WS
    // batches omit it (they append newer rows, never lower the floor).
    if (action.oldestSeq != null && state.oldestSeq === 0) {
      return { ...next, oldestSeq: action.oldestSeq };
    }
    return next;
  }
  if (action.kind === "prepend") {
    // Older history page. Reduce it in ISOLATION and prepend only its
    // activity rows; never re-fold the whole log, because live state
    // (optimistic prompt rows, locally-resolved approvals #1821, the
    // prompt queue, pendingConfigOption) is not a pure fold and would be
    // clobbered. Backward paging guarantees the page starts at a turn
    // boundary and ends just below the current oldest, so there is no
    // split-turn seam and no overlap to dedupe. See #2236.
    const next = { ...state, oldestSeq: action.oldestSeq };
    if (action.frames.length === 0) return next;
    next.activity = reduceFrames(action.frames).activity.concat(state.activity);
    return next;
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
    };
  }
  if (action.kind === "elicitation_resolved_locally") {
    // Optimistically drop the elicitation card once the server accepts the
    // resolution (204) or reports the nonce gone (404), instead of waiting
    // on the ElicitationResolved broadcast, which the seq dedupe can drop.
    const card = state.pendingElicitations.find((e) => e.nonce === action.nonce);
    const pendingElicitations = state.pendingElicitations.filter((e) => e.nonce !== action.nonce);
    const removed = pendingElicitations.length !== state.pendingElicitations.length;
    // Record the picked answer immediately (deduped by id), so it shows
    // even when the broadcast is dropped by seq dedupe. See #2209.
    const answers =
      card && action.resolution.action === "accept" ? summarizeAnswers(card, action.resolution.answers) : [];
    return {
      ...state,
      lastError: removed ? null : state.lastError,
      pendingElicitations,
      activity: appendElicitationAnswerRow(state.activity, action.nonce, answers),
    };
  }
  if (action.kind === "hydrate") {
    return action.state;
  }
  if (action.kind === "user_prompt") {
    // Bump `pendingUserPromptSeq` rather than touching `turnActive`
    // directly. `turnActive` derives from `pendingUserPromptSeq >
    // lastStoppedSeq`; the derived alias is recomputed here so any
    // existing `state.turnActive` reads stay consistent without a
    // selector hop. Without this the late `Stopped` from the prior
    // turn could clobber the spinner mid follow-up. See #1170.
    const pendingUserPromptSeq = state.pendingUserPromptSeq + 1;
    return {
      ...state,
      activity: state.activity.concat({
        id: `user-${Date.now()}-${state.activity.length}`,
        kind: "user_prompt",
        text: action.text,
        attachments: action.attachments && action.attachments.length > 0 ? action.attachments : undefined,
        at: new Date().toISOString(),
      }),
      assistantMessage: "",
      // A fresh prompt clears stale errors: the user has indicated
      // they're trying again, so don't keep nagging them.
      startupError: null,
      lastError: null,
      pendingUserPromptSeq,
      turnActive: isTurnActive({
        pendingUserPromptSeq,
        lastStoppedSeq: state.lastStoppedSeq,
      }),
    };
  }
  if (action.kind === "prompt_send_rejected") {
    // Optimistic submit already bumped pendingUserPromptSeq. When the
    // prompt POST is rejected client-side (for example unsupported
    // attachments), retire exactly one pending turn so Stop unlocks and
    // the composer returns to idle without waiting for a Stopped frame
    // that will never arrive.
    const lastStoppedSeq = Math.min(state.lastStoppedSeq + 1, state.pendingUserPromptSeq);
    return {
      ...state,
      inFlightTool: null,
      lastStoppedSeq,
      turnActive: isTurnActive({
        pendingUserPromptSeq: state.pendingUserPromptSeq,
        lastStoppedSeq,
      }),
    };
  }
  if (action.kind === "enqueue_prompt") {
    const entry: QueuedPrompt = {
      id: `q-${Date.now()}-${state.queuedPrompts.length}`,
      text: action.text,
      queuedAt: new Date().toISOString(),
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
  if (action.kind === "dequeue_prompts_by_id") {
    if (action.ids.length === 0) return state;
    const drop = new Set(action.ids);
    return {
      ...state,
      queuedPrompts: state.queuedPrompts.filter((q) => !drop.has(q.id)),
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
  if (action.kind === "dismiss_primer") {
    // Clear the offer entirely so it doesn't re-render on session
    // re-mount. A subsequent SessionContextReset re-seeds the field
    // with a new `resetSeq`, which the banner reads as a fresh
    // incident and shows again. See #1110.
    return { ...state, contextPrimerAvailable: null };
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
  // Mirror the worker state into a ref so the drain effect always sees
  // the latest value without re-running on every poll.
  const workerStateRef = useRef(workerState);
  useEffect(() => {
    workerStateRef.current = workerState;
  }, [workerState]);
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
  // Drain mode is sourced from the daemon's resolved `[acp]` config
  // and republished via `AcpPrefsProvider` (App.tsx). Held in a ref
  // so the drain effect's pop logic always sees the latest value
  // without re-running the effect on every toggle. See #1031.
  const { queueDrainMode, replayEvents } = useAcpPrefs();
  const drainModeRef = useRef(queueDrainMode);
  useEffect(() => {
    drainModeRef.current = queueDrainMode;
  }, [queueDrainMode]);
  // Clear-conversation aliases for the session's active agent. The drain
  // effect needs them to slice the queued-prompt snapshot at clear-command
  // boundaries so `/clear` (claude) / `/new` (codex, opencode) fires as a
  // standalone POST instead of being glued into a multi-paragraph combined
  // prompt; the server's `is_clear_command` is head-anchored on a trimmed
  // prompt, and an agent SDK receiving `/clear\n\n<follow-up>` does not
  // see a clean clear boundary. See #1356.
  const agentProfile = useAgentProfile();
  const clearAliasesRef = useRef(agentProfile.clearAliases);
  useEffect(() => {
    clearAliasesRef.current = agentProfile.clearAliases;
  }, [agentProfile.clearAliases]);
  // Mirror the server-side retention cap onto the reducer's
  // in-memory activity buffer. Without this, a frontend-only 200-row
  // cap clipped the rendered transcript regardless of what the user
  // set on the server side (#1111). 0 means unlimited.
  useEffect(() => {
    setActivityLimit(replayEvents);
  }, [replayEvents]);
  // Mirror status into a ref so sendPrompt's stable callback can short
  // circuit when the WS is closed without re-creating the callback on
  // every status flip (which would invalidate downstream memoised
  // handlers).
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
        const tailRes = await fetch(
          `/api/sessions/${encodeURIComponent(sid)}/acp/replay?before=${TAIL_BEFORE}&limit=${REPLAY_PAGE_SIZE}`,
          { credentials: "same-origin" },
        );
        if (!tailRes.ok) return;
        const tail = (await tailRes.json()) as ReplayPageResponse;
        if (tail.lost) {
          dispatch({ kind: "lagged", skipped: tail.highest_seq });
          return;
        }
        dispatch({ kind: "frames", frames: tail.frames ?? [], oldestSeq: tail.next_cursor ?? 0 });
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
        const res = await fetch(
          `/api/sessions/${encodeURIComponent(sid)}/acp/replay?since=${cursor}&limit=${REPLAY_PAGE_SIZE}`,
          { credentials: "same-origin" },
        );
        if (!res.ok) return;
        const data = (await res.json()) as ReplayPageResponse;
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
        if (data.frames.length > 0) {
          dispatch({ kind: "frames", frames: data.frames });
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
      const res = await fetch(
        `/api/sessions/${encodeURIComponent(sid)}/acp/replay?before=${before}&limit=${REPLAY_PAGE_SIZE}`,
        { credentials: "same-origin" },
      );
      if (!res.ok) return;
      const data = (await res.json()) as ReplayPageResponse;
      if ((data.frames ?? []).length > 0) {
        dispatch({ kind: "prepend", frames: data.frames, oldestSeq: data.next_cursor ?? before });
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
            const data = JSON.parse(ev.data) as AcpFrame | { kind: "lagged"; skipped?: number } | { kind: "heartbeat" };
            if (typeof data === "object" && data !== null && (data as { kind?: unknown }).kind === "heartbeat") {
              // Keepalive tick; liveness clock already bumped above.
              return;
            }
            if (
              typeof data === "object" &&
              data !== null &&
              "kind" in data &&
              (data as { kind?: unknown }).kind === "lagged"
            ) {
              const skipped = (data as unknown as { skipped?: number }).skipped ?? 0;
              dispatch({ kind: "lagged", skipped });
              // Try to recover via the snapshot endpoint.
              fetchReplay(sessionId);
              return;
            }
            if (typeof data === "object" && data !== null && "session_id" in data && "event" in data) {
              // Every incoming live frame is an "activity" tick for the
              // force-end-turn watchdog: as long as the agent is
              // streaming, the spinner stays "honest" and the escape
              // hatch doesn't appear. See WorkingSpinner in StructuredView.
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

  // Dispatch a prompt immediately, no queueing. Internal helper used by
  // both sendPrompt (when the turn is idle) and the drain effect below
  // (when popping the head of queuedPrompts on Stopped). The result tells
  // the drain effect what to do with the items it just sent:
  //   - "ok": delivered, retire them.
  //   - "non_retryable_failure": the server rejected them with a 4xx, so
  //     retrying would just re-POST the same failing batch every turn-end;
  //     retire them too (the error banner already surfaced the reason).
  //   - "retryable_failure": a transient disconnect / 5xx / network error,
  //     so keep the queue intact for the next turn-end retry.
  const dispatchPromptNow = useCallback(
    async (text: string, attachments?: PromptAttachmentInput[]): Promise<PromptSendResult> => {
      if (!sessionId) return "retryable_failure";
      if (statusRef.current !== "open") {
        dispatch({
          kind: "error",
          message: "Acp disconnected; message not sent. Reconnect to retry.",
        });
        return "retryable_failure";
      }
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
      // Optimistically echo the user's message; the agent reply
      // streams back as session/update events on the WS. If the POST
      // fails we'll surface a banner and the user can retry; the
      // optimistic row stays so they see what they tried to send.
      dispatch({
        kind: "user_prompt",
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
            dispatch({ kind: "prompt_send_rejected" });
          }
          if (!workerNotReady) {
            dispatch({
              kind: "error",
              message: `Could not send prompt (${res.status}). ${detail}`.trim(),
            });
          }
          return rejected ? "non_retryable_failure" : "retryable_failure";
        }
        return "ok";
      } catch (e) {
        dispatch({
          kind: "error",
          message: `Network error sending prompt: ${describeError(e)}`,
        });
        return "retryable_failure";
      }
    },
    [sessionId],
  );

  // Public sendPrompt. Enqueues locally whenever the session is not in
  // a state where an immediate POST would succeed; the drain effect
  // below dispatches once the session resumes. Inactive states covered:
  //   - WS not open (disconnected, reconnecting): #1359.
  //   - turn already in flight: #1031.
  //   - worker not in `running` (cold start, restart, stopped): #1088.
  //   - worker stopped or restarting at the session level: #1359.
  // Only when every gate clears does the prompt take the immediate POST
  // path. The guard set mirrors the drain effect below so the moment
  // the last gate flips the parked prompts fire. dispatchPromptNow keeps
  // its own status guard for the drain effect, which can race the WS
  // reopen window (see #1144).
  const sendPrompt = useCallback(
    async (text: string, attachments?: PromptAttachmentInput[]) => {
      if (!sessionId) return;
      // Auto-wake an archived or actively-snoozed session before
      // routing. The tmux send path runs this via
      // `Instance::touch_last_accessed` on the server side, but the
      // structured view composer enqueues locally while the worker is down
      // (which is true precisely BECAUSE the row is sunk), so the
      // server never sees the prompt and the flag stays set. Clear
      // it client-side; the reconciler picks the session back up on
      // its next ~2s tick and the queue drains as soon as a fresh
      // `AcpSessionAssigned` lands. See #1581.
      if (archivedAtRef.current || snoozedUntilRef.current) {
        const wakeResult = archivedAtRef.current
          ? await setSessionArchive(sessionId, false)
          : await setSessionSnooze(sessionId, null);
        if (!wakeResult) {
          // Wake PATCH failed (network drop / 5xx / 4xx). Surfacing
          // an error is the only safe move: enqueueing locally would
          // park the prompt in a queue that never drains, because
          // the reconciler keeps skipping the still-sunk session.
          // Route through the reducer's `error` action so the
          // existing `InteractionErrorBanner` renders it. See #1581.
          dispatch({
            kind: "error",
            message: "Could not wake this session. Please retry, or unarchive / unsnooze from the sidebar.",
          });
          return;
        }
      }
      const wsClosed = statusRef.current !== "open";
      const workerNotRunning = workerStateRef.current !== "running";
      // An idle-auto-stopped worker is dormant, not dead: the prompt POST
      // itself wakes it (the server clears dormancy, the reconciler
      // respawns, and `send_prompt`'s `wait_for_worker` holds the request
      // until the fresh worker is ready). So a dormant worker must NOT
      // park the prompt on `workerNotRunning` (the REST poll reads
      // "absent" until the respawn lands); parking would leave it in the
      // local queue forever and the worker would never come back. Only a
      // non-dormant cold worker (genuine mid-resume) still parks. See #1689.
      const blockedAsideFromWorker = wsClosed || state.turnActive || state.workerStopped || state.workerRestarting;
      const shouldEnqueue = state.workerIdleStopped
        ? blockedAsideFromWorker
        : blockedAsideFromWorker || workerNotRunning;
      if (shouldEnqueue) {
        // Park the prompt (with any attachments) so the drain effect
        // fires it once the agent is idle and connected, instead of
        // dropping the image. The attachment bytes ride the queued row
        // in memory only; `persistState` keeps them out of localStorage
        // (and drops the whole row on reload) so the quota invariant
        // holds. See #1833 / #1000.
        dispatch({ kind: "enqueue_prompt", text, attachments });
        // The agent was busy, so this prompt is genuinely queued. Report it for
        // opt-in telemetry (queue depth lives only in client state, so the
        // daemon cannot observe queueing on its own).
        reportAcpInteraction("prompt_queued");
        return;
      }
      const result = await dispatchPromptNow(text, attachments);
      // Idle-dormant direct send: the worker was respawning and did not
      // come online within send_prompt's wait window, so the POST returned
      // a retryable typed 503. Park the prompt (with its attachments)
      // instead of dropping it; the drain effect re-fires it once
      // AcpSessionAssigned brings the worker online. See #1748 / #1833.
      if (result === "retryable_failure" && state.workerIdleStopped) {
        dispatch({ kind: "enqueue_prompt", text, attachments });
        // Same parked-prompt telemetry as the busy-agent path above: the
        // idle-dormant wake POST failed retryably, so this prompt is queued
        // too and the daemon cannot observe it on its own.
        reportAcpInteraction("prompt_queued");
      }
    },
    [
      sessionId,
      state.turnActive,
      state.workerStopped,
      state.workerRestarting,
      state.workerIdleStopped,
      dispatchPromptNow,
    ],
  );

  // Drain effect: when the agent transitions to idle and the queue is
  // non-empty, dispatch follow-ups per `acp.queue_drain_mode`:
  //   - combined (default): join every queued entry with `\n\n` and
  //     fire one prompt; the agent's single response covers the batch.
  //   - serial: pop the head only; the next Stopped re-runs this effect
  //     and fires the following entry. One response per entry.
  // Guarded by `drainingRef` so a re-render between the dequeue
  // dispatch and the next state tick doesn't fire the same head twice.
  // Skipped while a worker-stopped / restarting banner is showing; a
  // fresh `AcpSessionAssigned` (which clears both flags) re-runs this
  // effect and drains then. See #1031.
  const drainingRef = useRef(false);
  useEffect(() => {
    if (drainingRef.current) return;
    if (!sessionIdRef.current) return;
    if (state.turnActive) return;
    if (state.workerStopped || state.workerRestarting) return;
    // Worker still mid-resume from a daemon cold start (or it never
    // came online). Park queued prompts so they don't POST into a
    // worker that's not online yet; the next REST poll flips
    // workerState to "running" and re-runs this effect. See #1088.
    //
    // Exception: an idle-auto-stopped (dormant) worker reads "absent"
    // too, but here the POST is the wake path, so we must let the drain
    // fire to issue it. The server's `touch_and_wake_if_sunk` clears
    // dormancy and `send_prompt`'s `wait_for_worker` holds the POST
    // until the respawned worker is ready. Without this, a prompt the
    // user queued while the worker was dormant would never drain. #1689.
    if (workerStateRef.current !== "running" && !state.workerIdleStopped) return;
    // Reconnect race: connect() awaits fetchReplay BEFORE opening the
    // WS, and replay can dispatch a Stopped frame that flips turnActive
    // off. If we drain here while the WS is still in "connecting",
    // dispatchPromptNow will bail (statusRef !== "open"), surface an
    // error banner, and (under the prior optimistic-clear ordering)
    // permanently drop the queued items. Park the drain until status
    // flips to "open"; the `status` value is in the dep array below
    // so this effect re-runs on transition. See #1144.
    if (statusRef.current !== "open") return;
    if (state.queuedPrompts.length === 0) return;
    drainingRef.current = true;
    if (drainModeRef.current === "combined") {
      // Slice the leading sub-batch out of the queue and POST only that.
      // The boundary is each clear-command alias (`/clear`, `/new`); when
      // the head is a clear alias it fires alone, otherwise the run of
      // non-clear entries up to the next alias is joined into one
      // combined POST. The remaining entries stay queued and the next
      // `Stopped` re-runs this effect, dispatching the next sub-batch.
      // Single-pass POST per drain matches the existing combined-mode
      // contract (one prompt fires per turn cycle), and the agent sees a
      // clean clear-command boundary instead of `/clear\n\n<text>`.
      // See #1356.
      //
      // The user can enqueue MORE prompts during the await; on success
      // we only clear the items in the snapshot so newly-typed entries
      // survive into the next turn. On failure (POST non-OK / network
      // blip / WS dropped mid-send) we leave the queue untouched so the
      // next Stopped retries.
      const queue = state.queuedPrompts;
      const aliases = clearAliasesRef.current;
      const headIsClear = aliases.length > 0 && isClearAlias(queue[0]!.text, aliases);
      let batchEnd = 1;
      if (!headIsClear && aliases.length > 0) {
        while (batchEnd < queue.length && !isClearAlias(queue[batchEnd]!.text, aliases)) {
          batchEnd += 1;
        }
      } else if (aliases.length === 0) {
        batchEnd = queue.length;
      }
      const snapshot = queue.slice(0, batchEnd);
      const combined = combineQueuedPrompts(snapshot);
      // Merge every attachment in the sub-batch into the one combined
      // POST. If that overflows the server's per-prompt cap the POST
      // 4xx-rejects and the batch retires via the non-retryable path
      // below, same as any other rejected combined send. See #1833.
      const combinedAttachments = snapshot.flatMap((q) => q.attachments ?? []);
      const sentIds = snapshot.map((q) => q.id);
      void dispatchPromptNow(combined, combinedAttachments.length > 0 ? combinedAttachments : undefined)
        .then((result) => {
          // Retire on success and on non-retryable rejection; only a
          // transient failure keeps the batch queued for the next retry.
          if (result !== "retryable_failure") {
            dispatch({ kind: "dequeue_prompts_by_id", ids: sentIds });
          }
        })
        .finally(() => {
          drainingRef.current = false;
        });
    } else {
      const head = state.queuedPrompts[0]!;
      const headId = head.id;
      void dispatchPromptNow(head.text, head.attachments)
        .then((result) => {
          if (result !== "retryable_failure") {
            dispatch({ kind: "dequeue_prompt", id: headId });
          }
        })
        .finally(() => {
          drainingRef.current = false;
        });
    }
  }, [
    status,
    workerState,
    state.turnActive,
    state.workerStopped,
    state.workerRestarting,
    state.workerIdleStopped,
    state.queuedPrompts,
    dispatchPromptNow,
  ]);

  const removeQueuedPrompt = useCallback((id: string) => {
    dispatch({ kind: "dequeue_prompt", id });
  }, []);

  const editQueuedPrompt = useCallback((id: string, text: string) => {
    dispatch({ kind: "edit_queued_prompt", id, text });
  }, []);

  const clearQueue = useCallback(() => {
    dispatch({ kind: "clear_queue" });
  }, []);

  const dismissPrimer = useCallback(() => {
    dispatch({ kind: "dismiss_primer" });
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

  return {
    state,
    status,
    /** True between an `onclose` and the next successful dial / failure
     *  to exhaust the retry envelope. Drives the banner's "Reconnecting
     *  (N/MAX) in Xs" copy. See #1130. */
    reconnecting,
    /** Current attempt number; 0 while the live socket is healthy,
     *  1..MAX while backing off. */
    retryCount,
    /** Seconds remaining before the next scheduled retry fires. The
     *  banner reads this on each render to animate the countdown. */
    retryCountdown,
    /** Maximum retries before falling back to the manual reconnect
     *  affordance. Exposed so the banner can render "N/MAX" without
     *  re-importing the constant. */
    maxRetries: ACP_MAX_RETRIES,
    /** User-triggered reconnect. Resets the retry counter and dials a
     *  fresh socket immediately. */
    manualReconnect,
    /** True once the WS has reached `onopen` at least once for the
     *  current session. Lets banner copy distinguish "first dial
     *  while the worker spawns" (no prior connection to recover) from
     *  "we lost a live connection and are retrying" (cached
     *  transcript and the recovery framing are honest). See #1106. */
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
    removeQueuedPrompt,
    editQueuedPrompt,
    clearQueue,
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

function describeError(e: unknown): string {
  if (e instanceof Error) return e.message;
  return String(e);
}
