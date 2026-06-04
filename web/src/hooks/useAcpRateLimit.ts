// Sidebar "rate-limited" indicator data source. Mirrors
// useQueuedCountForSessions (useAcpQueueCount.ts): it reads the
// per-session rate-limit info cached in acp-state storage and
// re-renders the caller only when one of the given session ids changes.
//
// A workspace row aggregates its sessions, so this reports how MANY of
// them are rate-limited plus the soonest reset time across them, letting
// the row show a count and a "resets at" hint without mounting any
// structured view hook.

import { useMemo, useSyncExternalStore } from "react";

import {
  getRateLimit,
  subscribeAcpState,
} from "../lib/acpStateStorage";

export interface SidebarRateLimit {
  /** How many of the given sessions are currently rate-limited. */
  count: number;
  /** Soonest `resets_at` across the rate-limited sessions, or null. */
  resetsAt: string | null;
}

// Encode the aggregate into a primitive so useSyncExternalStore's
// getSnapshot returns a stable value across renders (Object.is on a
// freshly built object would tear under React 18). The hook decodes it
// back into SidebarRateLimit via useMemo.
function snapshotFor(ids: readonly string[]): string {
  let count = 0;
  let soonest: string | null = null;
  for (const id of ids) {
    if (!id) continue;
    const info = getRateLimit(id);
    if (!info) continue;
    count += 1;
    if (soonest === null || info.resets_at < soonest) {
      soonest = info.resets_at;
    }
  }
  return `${count}|${soonest ?? ""}`;
}

// Returns the rate-limit aggregate for the given session ids, or null
// when none are rate-limited. Re-renders the caller only when one of
// THESE ids' rate-limit state changes.
export function useRateLimitedForSessions(
  sessionIds: readonly string[],
): SidebarRateLimit | null {
  const ids = sessionIds.join("|");
  const subscribe = useMemo(() => {
    const filter = new Set(ids ? ids.split("|").filter(Boolean) : []);
    return (cb: () => void) => subscribeAcpState(cb, filter);
  }, [ids]);
  const snapshot = useSyncExternalStore(
    subscribe,
    () => snapshotFor(ids ? ids.split("|") : []),
    () => "0|",
  );
  return useMemo(() => {
    const sep = snapshot.indexOf("|");
    const count = Number(snapshot.slice(0, sep));
    if (!count) return null;
    const resetsAt = snapshot.slice(sep + 1);
    return { count, resetsAt: resetsAt || null };
  }, [snapshot]);
}
