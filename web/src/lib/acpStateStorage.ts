// Key shape and TTL for persisted structured view state (`aoe:acp-state:v1:<id>`), plus the live queued counts
// the sidebar badge reads without importing the view hook.

import type { AcpState } from "./acpTypes";

export const STORAGE_KEY_PREFIX = "aoe:acp-state:v1:";
export const STATE_TTL_MS = 7 * 24 * 60 * 60 * 1000;

export interface PersistedEntry {
  savedAt: number;
  state: AcpState;
}

// Only while this browser's connection to the session is open and its queue came from the Daemon, so a
// persisted or other-tab copy never stands in for the Daemon's count.
const liveQueueCounts = new Map<string, number>();

type Listener = () => void;

const listeners = new Map<Listener, ReadonlySet<string> | null>();

/** Publish the live queue length, or `null` once the connection that knew it is gone. */
export function setLiveQueueCount(sessionId: string, count: number | null): void {
  if (count === null) {
    if (!liveQueueCounts.delete(sessionId)) return;
  } else {
    if (liveQueueCounts.get(sessionId) === count) return;
    liveQueueCounts.set(sessionId, count);
  }
  for (const [cb, filter] of listeners) {
    if (filter === null || filter.has(sessionId)) cb();
  }
}

export function getLiveQueueCount(sessionId: string): number | undefined {
  return liveQueueCounts.get(sessionId);
}

export function subscribeLiveQueueCounts(cb: Listener, filter: ReadonlySet<string> | null = null): () => void {
  listeners.set(cb, filter);
  return () => {
    listeners.delete(cb);
  };
}
