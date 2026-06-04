// Sidebar "N queued" badge data source. Mirrors useHasDraftForSessions
// (acpDrafts.ts) but returns the SUM of queued-prompt counts across
// the given session ids, so a workspace row reflects pending follow-ups
// for any of its sessions. Backed by the acp-state storage pub/sub;
// re-renders the caller only when the relevant counts change.

import { useMemo, useSyncExternalStore } from "react";

import {
  getQueuedCount,
  subscribeAcpState,
} from "../lib/acpStateStorage";

// Returns the total number of queued structured view follow-up prompts across the
// given session ids. Re-renders the caller only when one of THESE ids'
// counts changes, not on every acp-state write anywhere in the app.
export function useQueuedCountForSessions(
  sessionIds: readonly string[],
): number {
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
