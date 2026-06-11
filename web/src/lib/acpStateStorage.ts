// Storage layer for per-session structured view state. The structured view reducer state
// is mirrored into localStorage under `aoe:acp-state:v1:<id>` by
// useStructuredView's persistState so a reload rehydrates without replaying the
// whole transcript (see useStructuredView.ts and #1132). This module owns the
// key shape and TTL so both the writer (useStructuredView) and read-only
// consumers (the sidebar queued-prompt badge) share one source of truth,
// without the sidebar having to import the heavy structured view hook and its WS
// machinery just to read a count.
//
// It also exposes a small pub/sub (mirroring acpDrafts.ts) plus an
// in-memory queued-prompt count cache so the sidebar can render a live
// "N queued" badge via useSyncExternalStore. The cache keeps a snapshot
// read O(1) and avoids JSON.parsing the (potentially large) state blob
// during render: persistState already holds queuedPrompts.length and
// publishes it on every write, and cross-tab `storage` events parse the
// new value exactly once.

import type { AcpState, RateLimitInfo } from "./acpTypes";

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

// Queued-prompt count per session id, kept in sync by setQueueCount
// (same-tab writes) and the cross-tab storage listener. A missing entry
// means "not yet known this page load"; getQueuedCount lazily fills it
// from localStorage on first read so inactive sessions (no mounted
// structured view hook writing) still resolve a count.
const queueCounts = new Map<string, number>();

// Last-known rate-limit info per session id, kept in sync alongside
// queueCounts by setRateLimit (same-tab writes) and the cross-tab
// storage listener. Mirrors the queued-prompt cache so the sidebar can
// render a rate-limited indicator for a session whose structured view is
// not mounted. A stored `null` means "known, not rate-limited"; a
// missing (`undefined`) entry means "not yet read this page load", which
// getRateLimit lazily fills from localStorage.
const rateLimits = new Map<string, RateLimitInfo | null>();

type Listener = () => void;

// Each listener may register an optional id filter; null means "fire for
// any acp-state change" (used for a cross-tab localStorage.clear()).
const listeners = new Map<Listener, ReadonlySet<string> | null>();

function notify(sessionId: string | null): void {
  for (const [cb, filter] of listeners) {
    if (filter === null || sessionId === null || filter.has(sessionId)) cb();
  }
}

// Parse a queued-prompt count out of a raw persisted entry, honoring the
// TTL. Returns null when the entry is missing, expired, corrupt, or
// structurally invalid so callers fall back to 0 without caching a bogus
// value.
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

// Parse the rate-limit info out of a raw persisted entry, honoring the
// TTL. Returns null when the entry is missing, expired, corrupt, or has
// no (or malformed) rate-limit, so callers treat the session as not
// rate-limited rather than caching a bogus value.
function parseRateLimit(raw: string | null): RateLimitInfo | null {
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
    const rl = (parsed.state as Partial<AcpState> | undefined)?.rateLimit;
    if (rl && typeof rl.resets_at === "string" && typeof rl.kind === "string") {
      return rl;
    }
    return null;
  } catch {
    return null;
  }
}

// Publish a session's current queued-prompt count. Called by useStructuredView's
// persistState on every successful write; the length is already in hand
// there, so no JSON parsing happens on the write hot path.
export function setQueueCount(sessionId: string, count: number): void {
  queueCounts.set(sessionId, count);
  notify(sessionId);
}

// Publish a session's current rate-limit info (null when not rate
// limited). Called by useStructuredView's persistState on every successful
// write; the value is already in hand there, so no JSON parsing happens
// on the write hot path.
export function setRateLimit(sessionId: string, info: RateLimitInfo | null): void {
  rateLimits.set(sessionId, info);
  notify(sessionId);
}

// Drop a session's cached count and rate-limit (session delete / cache
// clear). With no argument, clears the whole cache.
export function clearQueueCount(sessionId?: string): void {
  if (sessionId === undefined) {
    queueCounts.clear();
    rateLimits.clear();
    notify(null);
    return;
  }
  queueCounts.delete(sessionId);
  rateLimits.delete(sessionId);
  notify(sessionId);
}

// Side-effect-free read of a session's queued-prompt count. Safe to call
// from a useSyncExternalStore snapshot during render: it never mutates
// localStorage (unlike useStructuredView's loadPersistedState, which prunes
// expired entries) and returns a primitive. Reads the in-memory cache
// first; on a miss it parses localStorage once and memoises the result.
export function getQueuedCount(sessionId: string): number {
  const cached = queueCounts.get(sessionId);
  if (cached !== undefined) return cached;
  if (typeof window === "undefined") return 0;
  let count: number;
  try {
    count = parseQueuedCount(window.localStorage.getItem(storageKey(sessionId))) ?? 0;
  } catch {
    // localStorage blocked/threw: don't memoise a transient failure.
    return 0;
  }
  queueCounts.set(sessionId, count);
  return count;
}

// Side-effect-free read of a session's last-known rate-limit info (null
// when not rate limited). Same contract as getQueuedCount: cache first,
// parse localStorage once on a miss and memoise.
export function getRateLimit(sessionId: string): RateLimitInfo | null {
  const cached = rateLimits.get(sessionId);
  if (cached !== undefined) return cached;
  if (typeof window === "undefined") return null;
  let info: RateLimitInfo | null;
  try {
    info = parseRateLimit(window.localStorage.getItem(storageKey(sessionId)));
  } catch {
    // localStorage blocked/threw: don't memoise a transient failure.
    return null;
  }
  rateLimits.set(sessionId, info);
  return info;
}

// Subscribe to acp-state changes. `filter` scopes the listener to a
// set of session ids; null receives every change. Fires for same-tab
// writes (via the notify in setQueueCount) and cross-tab writes (storage
// event). Returns an unsubscribe function. Mirrors subscribeDrafts.
export function subscribeAcpState(cb: Listener, filter: ReadonlySet<string> | null = null): () => void {
  listeners.set(cb, filter);
  const onStorage = (e: StorageEvent) => {
    // localStorage.clear() in another tab leaves e.key null; drop the
    // whole count cache and fire unconditionally.
    if (e.key === null) {
      queueCounts.clear();
      rateLimits.clear();
      cb();
      return;
    }
    const sid = sessionIdFromKey(e.key);
    if (sid === null) return;
    // Refresh both caches from the cross-tab value (parsed once each) so
    // the next snapshot read is consistent, then notify if in scope.
    queueCounts.set(sid, parseQueuedCount(e.newValue) ?? 0);
    rateLimits.set(sid, parseRateLimit(e.newValue));
    if (filter === null || filter.has(sid)) cb();
  };
  window.addEventListener("storage", onStorage);
  return () => {
    listeners.delete(cb);
    window.removeEventListener("storage", onStorage);
  };
}
