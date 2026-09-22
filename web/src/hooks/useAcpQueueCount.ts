import { useMemo, useSyncExternalStore } from "react";

import { getQueuedCount, subscribeAcpState } from "../lib/acpStateStorage";

/** Total queued structured-view prompts across `sessionIds`; re-renders only when their counts change. */
export function useQueuedCountForSessions(sessionIds: readonly string[]): number {
  // A primitive key keeps the snapshot stable across renders.
  const ids = sessionIds.join("|");
  const subscribe = useMemo(() => {
    const filter = new Set(ids.split("|").filter(Boolean));
    return (cb: () => void) => subscribeAcpState(cb, filter);
  }, [ids]);
  return useSyncExternalStore(
    subscribe,
    () => ids.split("|").reduce((total, id) => (id ? total + getQueuedCount(id) : total), 0),
    () => 0,
  );
}
