// Per-session ACP state: an in-memory LRU backed by versioned localStorage entries.

import { useCallback, useSyncExternalStore } from "react";
import { lastUserBoundaryIndex } from "../../lib/acpHistoryWindow";
import {
  emptyAcpState,
  normaliseTurnState,
  type AcpState,
  type ActivityRow,
  type BackgroundAgent,
  type BackgroundShell,
  type QueuedPrompt,
  type SubagentRecord,
  type WorkflowRun,
} from "../../lib/acpTypes";
import {
  PENDING_DONE_KEY_PREFIX,
  PENDING_KEY_PREFIX,
  STORAGE_KEY_PREFIX,
  STATE_TTL_MS,
  type PersistedEntry,
} from "../../lib/acpStateStorage";
import { safeSetItem } from "../../lib/safeStorage";

const STATE_CACHE_CAP = 32;
const stateCache = new Map<string, AcpState>();
const stateListeners = new Map<string, Set<() => void>>();

function storageKey(sessionId: string): string {
  return STORAGE_KEY_PREFIX + sessionId;
}

/** A stored entry with a usable timestamp, or null when it is missing or corrupt. */
function parseEntry(raw: string | null): PersistedEntry | null {
  if (raw === null) return null;
  try {
    const parsed = JSON.parse(raw) as PersistedEntry | null;
    return parsed && typeof parsed.savedAt === "number" && !Number.isNaN(parsed.savedAt) ? parsed : null;
  } catch {
    return null;
  }
}

function keysWithPrefix(prefix: string): string[] {
  const keys: string[] = [];
  for (let i = 0; i < window.localStorage.length; i++) {
    const k = window.localStorage.key(i);
    if (k?.startsWith(prefix)) keys.push(k);
  }
  return keys;
}

const persistedKeys = () => keysWithPrefix(STORAGE_KEY_PREFIX);

// Only ever touches `aoe:acp-state:v1:*`: drafts and other keys are authoritative and must survive.
export function evictOldestPersistedAcpState(currentKey: string): boolean {
  if (typeof window === "undefined") return false;
  try {
    let oldestKey: string | null = null;
    let oldestTime = Infinity;
    let firstCorruptKey: string | null = null;
    for (const k of persistedKeys()) {
      if (k === currentKey) continue;
      const raw = window.localStorage.getItem(k);
      if (raw === null) continue;
      const parsed = parseEntry(raw);
      if (!parsed) firstCorruptKey ??= k;
      else if (parsed.savedAt < oldestTime) {
        oldestTime = parsed.savedAt;
        oldestKey = k;
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

// Optimistic rows and in-flight ids are ephemeral, and attachment bytes would blow the quota
// (text alone would drain a degraded prompt on reload), so none of them are persisted.
function toPersistedState(state: AcpState, durable: (id: string) => boolean): AcpState {
  const base: AcpState =
    state.optimisticRows.length > 0 ||
    state.inflightPromptIds.length > 0 ||
    state.restartInFlight ||
    state.restartStoppedSent ||
    state.restartFailure !== null ||
    state.retriedError !== null
      ? {
          ...state,
          optimisticRows: [],
          inflightPromptIds: [],
          restartInFlight: false,
          restartStoppedSent: false,
          restartFailure: null,
          retriedError: null,
        }
      : state;
  // A pending row leaves the transcript only once its own entry (or its terminal mark) is durable.
  const dropped = (q: QueuedPrompt) => !!q.attachments?.length || (q.pending === true && durable(q.id));
  if (!base.queuedPrompts.some(dropped)) return base;
  return { ...base, queuedPrompts: base.queuedPrompts.filter((q) => !dropped(q)) };
}

// An unconfirmed queued prompt is the only retry copy. Each lives in its own entry, written at once and never
// part of the throttled transcript, so a stale snapshot or a concurrent tab can neither erase nor resurrect it.
interface PendingEntry {
  savedAt: number;
  row: QueuedPrompt;
}

const pendingPrefix = (sessionId: string) => `${PENDING_KEY_PREFIX}${sessionId}:`;
// Written when a tab confirms or removes a row, so a legacy transcript copy is never adopted again.
const donePrefix = (sessionId: string) => `${PENDING_DONE_KEY_PREFIX}${sessionId}:`;

function hasStored(key: string): boolean {
  try {
    return window.localStorage.getItem(key) !== null;
  } catch {
    return false;
  }
}

function readPendingRows(sessionId: string): QueuedPrompt[] {
  const rows: QueuedPrompt[] = [];
  try {
    for (const k of keysWithPrefix(pendingPrefix(sessionId))) {
      const parsed = JSON.parse(window.localStorage.getItem(k) ?? "null") as PendingEntry | null;
      if (parsed?.row && Date.now() - parsed.savedAt <= STATE_TTL_MS) rows.push(parsed.row);
    }
  } catch {
    // Storage unavailable or corrupt.
  }
  return rows.sort((a, b) => a.queuedAt.localeCompare(b.queuedAt));
}

// Rows with attachments are never persisted: their bytes would blow the quota.
function pendingRowsOf(state: AcpState): QueuedPrompt[] {
  return state.queuedPrompts.filter((q) => q.pending && !q.attachments?.length);
}

// Per session, the ids this tab saw pending at its last sync (so it removes only rows it removed itself) and
// the text it has durably written for each (so a failed write is retried and an edit is rewritten).
const knownPendingIds = new Map<string, ReadonlySet<string>>();
const writtenPendingText = new Map<string, Map<string, string>>();

function writePendingRow(sessionId: string, row: QueuedPrompt): boolean {
  const key = pendingPrefix(sessionId) + row.id;
  const body = JSON.stringify({ savedAt: Date.now(), row } satisfies PendingEntry);
  const ok = safeSetItem(key, body) || (evictOldestPersistedAcpState(storageKey(sessionId)) && safeSetItem(key, body));
  if (ok) {
    const written = writtenPendingText.get(sessionId) ?? new Map<string, string>();
    written.set(row.id, row.text);
    writtenPendingText.set(sessionId, written);
  }
  return ok;
}

function writeDoneMark(sessionId: string, id: string): boolean {
  const key = donePrefix(sessionId) + id;
  const body = JSON.stringify({ savedAt: Date.now() });
  return safeSetItem(key, body) || (evictOldestPersistedAcpState(storageKey(sessionId)) && safeSetItem(key, body));
}

function syncPendingRows(sessionId: string, state: AcpState): void {
  if (typeof window === "undefined") return;
  const rows = pendingRowsOf(state);
  const ids = new Set(rows.map((q) => q.id));
  const written = writtenPendingText.get(sessionId);
  const known = new Set(ids);
  try {
    for (const id of knownPendingIds.get(sessionId) ?? []) {
      if (ids.has(id)) continue;
      // The terminal mark goes first; until it is stored the row stays tracked and its entry stays, so this retries.
      if (!writeDoneMark(sessionId, id)) {
        known.add(id);
        continue;
      }
      window.localStorage.removeItem(pendingPrefix(sessionId) + id);
      written?.delete(id);
    }
    for (const row of rows) {
      const stored = written?.get(row.id);
      if (stored === row.text) continue;
      // An edit must not recreate a row another tab already confirmed or removed.
      if (stored !== undefined && window.localStorage.getItem(pendingPrefix(sessionId) + row.id) === null) continue;
      writePendingRow(sessionId, row);
    }
  } catch {
    // Storage unavailable.
  }
  knownPendingIds.set(sessionId, known);
}

/** The state with its unconfirmed rows taken from their own entries; the stored list never carries pending rows. */
function withStoredPendingRows(sessionId: string, state: AcpState): AcpState {
  const stored = new Map(readPendingRows(sessionId).map((q) => [q.id, q]));
  if (stored.size === 0) return state;
  const queued = state.queuedPrompts.map((q) => (q.pending && stored.has(q.id) ? stored.get(q.id)! : q));
  const known = new Set(queued.map((q) => q.id));
  return { ...state, queuedPrompts: [...queued, ...[...stored.values()].filter((q) => !known.has(q.id))] };
}

// Rows an older bundle left only in the transcript get their own entry; one another tab already finished is dropped.
function adoptPendingRows(sessionId: string, state: AcpState): AcpState {
  const finished = (q: QueuedPrompt) => q.pending === true && hasStored(donePrefix(sessionId) + q.id);
  const live = state.queuedPrompts.some(finished)
    ? { ...state, queuedPrompts: state.queuedPrompts.filter((q) => !finished(q)) }
    : state;
  if (knownPendingIds.has(sessionId)) return live;
  const rows = pendingRowsOf(live);
  knownPendingIds.set(sessionId, new Set(rows.map((q) => q.id)));
  const stored = new Set(readPendingRows(sessionId).map((q) => q.id));
  const written = new Map<string, string>();
  writtenPendingText.set(sessionId, written);
  for (const row of rows) {
    if (stored.has(row.id)) written.set(row.id, row.text);
    else writePendingRow(sessionId, row);
  }
  return live;
}

export function persistState(sessionId: string, state: AcpState, savedAt = Date.now()): void {
  const key = storageKey(sessionId);
  const durable = (id: string) => hasStored(pendingPrefix(sessionId) + id) || hasStored(donePrefix(sessionId) + id);
  const body = JSON.stringify({ savedAt, state: toPersistedState(state, durable) } satisfies PersistedEntry);
  // On a quota failure evict one entry and retry once; the cache is best effort.
  if (!safeSetItem(key, body) && evictOldestPersistedAcpState(key)) safeSetItem(key, body);
}

// Rows with no user prompt and older history pending would open mid-turn. Drop the rows and cursor so replay
// reloads the transcript from the tail; folded control state keeps its own cursor and is untouched.
function reopenState(state: AcpState): AcpState {
  if (state.oldestSeq > 1 && lastUserBoundaryIndex(state.activity) < 0) return { ...state, activity: [], oldestSeq: 0 };
  return state;
}

export function loadPersistedState(sessionId: string): AcpState | undefined {
  if (typeof window === "undefined") return undefined;
  try {
    const parsed = parseEntry(window.localStorage.getItem(storageKey(sessionId)));
    let base: AcpState | undefined;
    if (parsed && typeof parsed.state === "object" && parsed.state !== null) {
      const state = parsed.state as Partial<AcpState>;
      if (
        Date.now() - parsed.savedAt > STATE_TTL_MS ||
        typeof state.lastSeq !== "number" ||
        !Array.isArray(state.activity) ||
        !Array.isArray(state.queuedPrompts)
      ) {
        window.localStorage.removeItem(storageKey(sessionId));
      } else {
        // Merge over defaults so entries from an older bundle gain newly added fields.
        base = { ...emptyAcpState(), ...(state as AcpState) };
      }
    }
    // Unconfirmed rows outlive a missing or evicted transcript.
    const loaded = adoptPendingRows(sessionId, withStoredPendingRows(sessionId, base ?? emptyAcpState()));
    if (!base && loaded.queuedPrompts.length === 0) return undefined;
    return reopenState(normaliseTurnState(loaded));
  } catch {
    return undefined;
  }
}

function removePersisted(keys: () => string[]): void {
  if (typeof window === "undefined") return;
  try {
    for (const k of keys()) window.localStorage.removeItem(k);
  } catch {
    // Storage unavailable.
  }
}

let sweptStorage = false;
export function sweepExpiredStorage(): void {
  if (sweptStorage) return;
  sweptStorage = true;
  const now = Date.now();
  for (const keys of [
    persistedKeys,
    () => keysWithPrefix(PENDING_KEY_PREFIX),
    () => keysWithPrefix(PENDING_DONE_KEY_PREFIX),
  ]) {
    removePersisted(() =>
      keys().filter((k) => {
        const parsed = parseEntry(window.localStorage.getItem(k));
        return !parsed || now - parsed.savedAt > STATE_TTL_MS;
      }),
    );
  }
}

function lruInsert(sessionId: string, value: AcpState): void {
  stateCache.delete(sessionId);
  stateCache.set(sessionId, value);
  while (stateCache.size > STATE_CACHE_CAP) {
    const oldest = stateCache.keys().next().value;
    if (oldest === undefined) break;
    stateCache.delete(oldest);
  }
}

export function cacheGet(sessionId: string): AcpState | undefined {
  const memory = stateCache.get(sessionId);
  // An evicted session can still have unflushed state, newer than anything in storage.
  const known = memory ?? pendingPersist.get(sessionId)?.state;
  const value = known ? reopenState(known) : loadPersistedState(sessionId);
  if (value === undefined) return undefined;
  lruInsert(sessionId, value);
  // Wake subscribers that rendered before the cache was primed or saw a pre-reset entry; deferred so it never fires mid-render.
  if (value !== memory) queueMicrotask(() => notifyStateListeners(sessionId));
  return value;
}

const PERSIST_THROTTLE_MS = 2000;
// `at` is when the snapshot was last captured, to detect another tab writing after it.
const pendingPersist = new Map<string, { state: AcpState; at: number; timer: ReturnType<typeof setTimeout> }>();

function schedulePersist(sessionId: string, state: AcpState): void {
  const pending = pendingPersist.get(sessionId);
  if (pending) {
    pending.state = state;
    pending.at = Date.now();
    return;
  }
  const timer = setTimeout(() => flushPersist(sessionId), PERSIST_THROTTLE_MS);
  pendingPersist.set(sessionId, { state, at: Date.now(), timer });
}

function storedSavedAt(sessionId: string): number {
  try {
    return parseEntry(window.localStorage.getItem(storageKey(sessionId)))?.savedAt ?? -Infinity;
  } catch {
    return -Infinity;
  }
}

/** Persist pending state now, for one session or all. */
export function flushPersist(sessionId?: string): void {
  for (const id of sessionId === undefined ? [...pendingPersist.keys()] : [sessionId]) {
    const pending = pendingPersist.get(id);
    if (!pending) continue;
    clearTimeout(pending.timer);
    pendingPersist.delete(id);
    // Entries carry capture time, so the newer capture wins whichever tab flushes first.
    if (storedSavedAt(id) > pending.at) continue;
    persistState(id, pending.state, pending.at);
  }
}

// localStorage only needs the latest state by the time the page is hidden or unloaded.
if (typeof window !== "undefined" && typeof document !== "undefined") {
  window.addEventListener("pagehide", () => flushPersist());
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "hidden") flushPersist();
  });
}

export function cacheSet(sessionId: string, value: AcpState): void {
  lruInsert(sessionId, value);
  syncPendingRows(sessionId, value);
  schedulePersist(sessionId, value);
  notifyStateListeners(sessionId);
}

function notifyStateListeners(sessionId: string): void {
  for (const cb of stateListeners.get(sessionId) ?? []) cb();
}

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

const EMPTY_ACTIVITY: ActivityRow[] = [];
const EMPTY_BACKGROUND_AGENTS: BackgroundAgent[] = [];
const EMPTY_BACKGROUND_TASKS: string[] = [];
const EMPTY_SUBAGENTS: SubagentRecord[] = [];
const EMPTY_BACKGROUND_SHELLS: BackgroundShell[] = [];
const EMPTY_WORKFLOW_RUNS: Record<string, WorkflowRun> = {};

/** One field of a session's cached state, so sibling panels need no second WebSocket. */
function useCachedField<T>(sessionId: string | null, pick: (state: AcpState) => T, empty: T): T {
  const subscribe = useCallback(
    (cb: () => void) => (sessionId ? subscribeAcpState(sessionId, cb) : () => {}),
    [sessionId],
  );
  const getSnapshot = useCallback(() => {
    const state = sessionId ? stateCache.get(sessionId) : undefined;
    return state ? pick(state) : empty;
  }, [sessionId, pick, empty]);
  return useSyncExternalStore(subscribe, getSnapshot);
}

const pickActivity = (s: AcpState) => s.activity;
const pickBackgroundAgents = (s: AcpState) => s.backgroundAgents;
const pickSubagents = (s: AcpState) => s.subagents;
const pickBackgroundTasks = (s: AcpState) => s.backgroundTasks;
const pickBackgroundShells = (s: AcpState) => s.backgroundShells;
const pickWorkflowRuns = (s: AcpState) => s.workflowRuns;

export function useActivity(sessionId: string | null): ActivityRow[] {
  return useCachedField(sessionId, pickActivity, EMPTY_ACTIVITY);
}

export function useBackgroundAgents(sessionId: string | null): BackgroundAgent[] {
  return useCachedField(sessionId, pickBackgroundAgents, EMPTY_BACKGROUND_AGENTS);
}

export function useSubagents(sessionId: string | null): SubagentRecord[] {
  return useCachedField(sessionId, pickSubagents, EMPTY_SUBAGENTS);
}

export function useBackgroundTasks(sessionId: string | null): string[] {
  return useCachedField(sessionId, pickBackgroundTasks, EMPTY_BACKGROUND_TASKS);
}

export function useBackgroundShells(sessionId: string | null): BackgroundShell[] {
  return useCachedField(sessionId, pickBackgroundShells, EMPTY_BACKGROUND_SHELLS);
}

export function useWorkflowRuns(sessionId: string | null): Record<string, WorkflowRun> {
  return useCachedField(sessionId, pickWorkflowRuns, EMPTY_WORKFLOW_RUNS);
}

/** Drop one session's cached state, or all of it, so a reused id never shows a prior transcript. */
export function clearAcpCache(sessionId?: string): void {
  for (const id of sessionId === undefined ? [...pendingPersist.keys()] : [sessionId]) {
    const pending = pendingPersist.get(id);
    if (pending) clearTimeout(pending.timer);
    pendingPersist.delete(id);
  }
  if (sessionId === undefined) {
    stateCache.clear();
    knownPendingIds.clear();
    writtenPendingText.clear();
    removePersisted(persistedKeys);
    removePersisted(() => keysWithPrefix(PENDING_KEY_PREFIX));
    removePersisted(() => keysWithPrefix(PENDING_DONE_KEY_PREFIX));
  } else {
    stateCache.delete(sessionId);
    knownPendingIds.delete(sessionId);
    writtenPendingText.delete(sessionId);
    removePersisted(() => [
      storageKey(sessionId),
      ...keysWithPrefix(pendingPrefix(sessionId)),
      ...keysWithPrefix(donePrefix(sessionId)),
    ]);
  }
}
