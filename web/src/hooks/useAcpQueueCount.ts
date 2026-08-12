// Sidebar "N queued" badge data source. Mirrors useHasDraftForSessions
// (acpDrafts.ts) but returns the SUM of queued-prompt counts across
// the given session ids, so a workspace row reflects pending follow-ups
// for any of its sessions. Backed by the acp-state storage pub/sub;
// re-renders the caller only when the relevant counts change.

import { useMemo, useSyncExternalStore } from "react";

import { isArmedForBackgroundDrain } from "../lib/acpDrainCoordinator";
import { getQueuedCount, subscribeAcpState } from "../lib/acpStateStorage";

// Returns the total number of queued structured view follow-up prompts across the
// given session ids. Re-renders the caller only when one of THESE ids'
// counts changes, not on every acp-state write anywhere in the app.
export function useQueuedCountForSessions(sessionIds: readonly string[]): number {
  // Stable join key so getSnapshot returns the same primitive across
  // renders unless the relevant counts actually change; otherwise
  // useSyncExternalStore would tear under React 18's strict checks.
  const ids = sessionIds.join("|");
  const subscribe = useMemo(() => {
    const filter = new Set(ids ? ids.split("|").filter(Boolean) : []);
    return (cb: () => void) => subscribeAcpState(cb, filter);
  }, [ids]);
  return useSyncExternalStore(
    subscribe,
    () => {
      let total = 0;
      for (const id of ids ? ids.split("|") : []) {
        if (id) total += getQueuedCount(id);
      }
      return total;
    },
    () => 0,
  );
}

/** How many sessions may hold a background drain subscription at once.
 *  Each one costs a WebSocket plus a replay top-up, and a queue can
 *  accumulate across many sessions, so this bounds the fan-out rather
 *  than trusting the queue count to stay small. Sessions past the cap
 *  keep their queue and their badge; they drain as earlier ones finish or
 *  when the user opens them. */
export const MAX_BACKGROUND_DRAINERS = 3;

/** Pick which of `candidateIds` should get a headless drain subscription:
 *  those with a non-empty queue that is armed for background delivery,
 *  capped at `MAX_BACKGROUND_DRAINERS`. Callers pass only sessions whose
 *  chat is NOT mounted, since a visible chat already owns its own hook.
 *  See #3331. */
export function useBackgroundDrainSessionIds(candidateIds: readonly string[]): string[] {
  // Same stable-primitive contract as useQueuedCountForSessions: the
  // snapshot returns a joined string so useSyncExternalStore never tears
  // on a fresh array identity.
  const ids = candidateIds.join("|");
  const subscribe = useMemo(() => {
    const filter = new Set(ids ? ids.split("|").filter(Boolean) : []);
    return (cb: () => void) => subscribeAcpState(cb, filter);
  }, [ids]);
  const picked = useSyncExternalStore(
    subscribe,
    () => {
      const out: string[] = [];
      for (const id of ids ? ids.split("|") : []) {
        if (out.length >= MAX_BACKGROUND_DRAINERS) break;
        if (!id || getQueuedCount(id) === 0) continue;
        if (!isArmedForBackgroundDrain(id)) continue;
        out.push(id);
      }
      return out.join("|");
    },
    () => "",
  );
  return useMemo(() => (picked ? picked.split("|") : []), [picked]);
}
