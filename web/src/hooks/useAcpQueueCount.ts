import { useMemo, useSyncExternalStore } from "react";

import { getLiveQueueCount, subscribeLiveQueueCounts } from "../lib/acpStateStorage";
import type { SessionResponse } from "../lib/types";

/** Each session's queued count: this browser's while it holds the live queue, otherwise the Daemon's. */
export function useQueuedCounts(sessions: readonly SessionResponse[]): number[] {
  // Primitive keys keep the subscription and snapshot stable across renders.
  const ids = sessions.map((s) => s.id).join("|");
  const served = sessions.map((s) => s.queued_count ?? 0).join("|");
  const subscribe = useMemo(() => {
    const filter = new Set(ids.split("|").filter(Boolean));
    return (cb: () => void) => subscribeLiveQueueCounts(cb, filter);
  }, [ids]);
  const snapshot = useSyncExternalStore(
    subscribe,
    () => {
      if (!ids) return "";
      const fallback = served.split("|");
      return ids
        .split("|")
        .map((id, i) => getLiveQueueCount(id) ?? Number(fallback[i]))
        .join("|");
    },
    () => served,
  );
  return useMemo(() => (snapshot ? snapshot.split("|").map(Number) : []), [snapshot]);
}
