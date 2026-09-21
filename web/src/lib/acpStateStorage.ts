// Key shape and TTL for persisted structured view state (`aoe:acp-state:v1:<id>`), plus a queued-count cache and pub/sub so the sidebar badge avoids parsing state blobs or importing the view hook.

import type { AcpState } from "./acpTypes";

export const STORAGE_KEY_PREFIX = "aoe:acp-state:v1:";
export const STATE_TTL_MS = 7 * 24 * 60 * 60 * 1000;

export interface PersistedEntry {
  savedAt: number;
  state: AcpState;
}

function storageKey(sessionId: string): string {
  return STORAGE_KEY_PREFIX + sessionId;
}

function sessionIdFromKey(key: string): string | null {
  if (!key.startsWith(STORAGE_KEY_PREFIX)) return null;
  return key.slice(STORAGE_KEY_PREFIX.length);
}

// Filled by same-tab writes and cross-tab events, or lazily from localStorage on first read.
const queueCounts = new Map<string, number>();

type Listener = () => void;

const listeners = new Map<Listener, ReadonlySet<string> | null>();

function notify(sessionId: string | null): void {
  for (const [cb, filter] of listeners) {
    if (filter === null || sessionId === null || filter.has(sessionId)) cb();
  }
}

// Null for a missing, expired, or invalid entry, so callers fall back to 0 without caching it.
function parseQueuedCount(raw: string | null): number | null {
  if (raw === null) return null;
  try {
    const parsed = JSON.parse(raw) as PersistedEntry | null;
    if (
      !parsed ||
      typeof parsed.savedAt !== "number" ||
      Number.isNaN(parsed.savedAt) ||
      Date.now() - parsed.savedAt > STATE_TTL_MS
    ) {
      return null;
    }
    const q = (parsed.state as Partial<AcpState> | undefined)?.queuedPrompts;
    return Array.isArray(q) ? q.length : null;
  } catch {
    return null;
  }
}

// Newest `queuedAt` in epoch ms, honoring the TTL; null when absent.
function parseNewestQueuedAt(raw: string | null): number | null {
  if (raw === null) return null;
  try {
    const parsed = JSON.parse(raw) as PersistedEntry | null;
    if (
      !parsed ||
      typeof parsed.savedAt !== "number" ||
      Number.isNaN(parsed.savedAt) ||
      Date.now() - parsed.savedAt > STATE_TTL_MS
    ) {
      return null;
    }
    const q = (parsed.state as Partial<AcpState> | undefined)?.queuedPrompts;
    if (!Array.isArray(q)) return null;
    let newest: number | null = null;
    for (const row of q) {
      const at = Date.parse(row?.queuedAt ?? "");
      if (Number.isNaN(at)) continue;
      if (newest === null || at > newest) newest = at;
    }
    return newest;
  } catch {
    return null;
  }
}

export function setQueueCount(sessionId: string, count: number): void {
  queueCounts.set(sessionId, count);
  notify(sessionId);
}

export function clearQueueCount(sessionId?: string): void {
  if (sessionId === undefined) {
    queueCounts.clear();
    notify(null);
    return;
  }
  queueCounts.delete(sessionId);
  notify(sessionId);
}

// Side-effect free, so safe in a useSyncExternalStore snapshot.
export function getQueuedCount(sessionId: string): number {
  const cached = queueCounts.get(sessionId);
  if (cached !== undefined) return cached;
  if (typeof window === "undefined") return 0;
  let count: number;
  try {
    count = parseQueuedCount(window.localStorage.getItem(storageKey(sessionId))) ?? 0;
  } catch {
    // Don't memoise a transient failure.
    return 0;
  }
  queueCounts.set(sessionId, count);
  return count;
}

// Lets the background drain tell a just-parked queue from one restored days later. Uncached.
export function getNewestQueuedAt(sessionId: string): number | null {
  if (typeof window === "undefined") return null;
  try {
    return parseNewestQueuedAt(window.localStorage.getItem(storageKey(sessionId)));
  } catch {
    return null;
  }
}

export function subscribeAcpState(cb: Listener, filter: ReadonlySet<string> | null = null): () => void {
  listeners.set(cb, filter);
  const onStorage = (e: StorageEvent) => {
    // A null key means localStorage.clear() in another tab.
    if (e.key === null) {
      queueCounts.clear();
      cb();
      return;
    }
    const sid = sessionIdFromKey(e.key);
    if (sid === null) return;
    queueCounts.set(sid, parseQueuedCount(e.newValue) ?? 0);
    if (filter === null || filter.has(sid)) cb();
  };
  window.addEventListener("storage", onStorage);
  return () => {
    listeners.delete(cb);
    window.removeEventListener("storage", onStorage);
  };
}
