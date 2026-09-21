// Per-session ACP state: an in-memory LRU backed by versioned localStorage entries.

import { useCallback, useSyncExternalStore } from "react";
import { emptyAcpState, normaliseTurnState, type AcpState, type BackgroundAgent } from "../../lib/acpTypes";
import {
  STORAGE_KEY_PREFIX,
  STATE_TTL_MS,
  clearQueueCount,
  setQueueCount,
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

function persistedKeys(): string[] {
  const keys: string[] = [];
  for (let i = 0; i < window.localStorage.length; i++) {
    const k = window.localStorage.key(i);
    if (k?.startsWith(STORAGE_KEY_PREFIX)) keys.push(k);
  }
  return keys;
}

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
function toPersistedState(state: AcpState): AcpState {
  const base: AcpState =
    state.optimisticRows.length > 0 || state.inflightPromptIds.length > 0
      ? { ...state, optimisticRows: [], inflightPromptIds: [] }
      : state;
  if (!base.queuedPrompts.some((q) => q.attachments?.length)) return base;
  return { ...base, queuedPrompts: base.queuedPrompts.filter((q) => !q.attachments?.length) };
}

export function persistState(sessionId: string, state: AcpState): void {
  const key = storageKey(sessionId);
  const body = JSON.stringify({ savedAt: Date.now(), state: toPersistedState(state) } satisfies PersistedEntry);
  // On a quota failure evict one entry and retry once; the cache is best effort.
  if (safeSetItem(key, body) || (evictOldestPersistedAcpState(key) && safeSetItem(key, body))) {
    setQueueCount(sessionId, state.queuedPrompts.length);
  }
}

export function loadPersistedState(sessionId: string): AcpState | undefined {
  if (typeof window === "undefined") return undefined;
  try {
    const parsed = parseEntry(window.localStorage.getItem(storageKey(sessionId)));
    if (!parsed || typeof parsed.state !== "object" || parsed.state === null) return undefined;
    const state = parsed.state as Partial<AcpState>;
    if (
      Date.now() - parsed.savedAt > STATE_TTL_MS ||
      typeof state.lastSeq !== "number" ||
      !Array.isArray(state.activity) ||
      !Array.isArray(state.queuedPrompts)
    ) {
      window.localStorage.removeItem(storageKey(sessionId));
      return undefined;
    }
    // Merge over defaults so entries from an older bundle gain newly added fields.
    return normaliseTurnState({ ...emptyAcpState(), ...(state as AcpState) });
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
  removePersisted(() =>
    persistedKeys().filter((k) => {
      const parsed = parseEntry(window.localStorage.getItem(k));
      return !parsed || now - parsed.savedAt > STATE_TTL_MS;
    }),
  );
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
  const value = stateCache.get(sessionId) ?? loadPersistedState(sessionId);
  if (value === undefined) return undefined;
  const fromStorage = !stateCache.has(sessionId);
  lruInsert(sessionId, value);
  // Wake subscribers that rendered before the cache was primed; deferred so it never fires mid-render.
  if (fromStorage) queueMicrotask(() => notifyStateListeners(sessionId));
  return value;
}

export function cacheSet(sessionId: string, value: AcpState): void {
  lruInsert(sessionId, value);
  persistState(sessionId, value);
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

const EMPTY_BACKGROUND_AGENTS: BackgroundAgent[] = [];

/** Background agents for a session, read from the cache so sibling panels need no second WebSocket. */
export function useBackgroundAgents(sessionId: string | null): BackgroundAgent[] {
  const subscribe = useCallback(
    (cb: () => void) => (sessionId ? subscribeAcpState(sessionId, cb) : () => {}),
    [sessionId],
  );
  const getSnapshot = useCallback(
    () => (sessionId ? stateCache.get(sessionId)?.backgroundAgents : undefined) ?? EMPTY_BACKGROUND_AGENTS,
    [sessionId],
  );
  return useSyncExternalStore(subscribe, getSnapshot);
}

/** Drop one session's cached state, or all of it, so a reused id never shows a prior transcript. */
export function clearAcpCache(sessionId?: string): void {
  if (sessionId === undefined) {
    stateCache.clear();
    removePersisted(persistedKeys);
  } else {
    stateCache.delete(sessionId);
    removePersisted(() => [storageKey(sessionId)]);
  }
  clearQueueCount(sessionId);
}
