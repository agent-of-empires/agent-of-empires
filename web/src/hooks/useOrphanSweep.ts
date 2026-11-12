import { useEffect, useRef } from "react";
import { fetchAllSessions } from "../lib/api";
import { sweepOrphanDrafts } from "../lib/acpDrafts";
import { sweepOrphanComments } from "../components/diff/comments/storage";
import type { SessionResponse } from "../lib/types";

/**
 * Once the first list settles, drops drafts and diff comments (#1358, #1842) of sessions deleted elsewhere,
 * and empty keys written before the empty-removal fix.
 *
 * The list omits the trash, but a trashed session still owns its drafts and comments. When the trash is not
 * empty the ids therefore come from one complete `state=all` snapshot: two scoped reads could straddle a
 * restore and miss the session in both. A failed read skips the sweep. The locally known ids are added after
 * that read, so a session created meanwhile is not swept.
 */
export function useOrphanSweep(sessions: SessionResponse[], sessionsLoaded: boolean, trashedCount: number) {
  const swept = useRef(false);
  const latest = useRef(sessions);
  useEffect(() => {
    latest.current = sessions;
  }, [sessions]);
  useEffect(() => {
    /* eslint-disable react-you-might-not-need-an-effect/no-event-handler -- one-shot when the first list settles */
    if (swept.current || !sessionsLoaded) return;
    swept.current = true;
    const hasTrash = trashedCount > 0;
    void (async () => {
      const inventory: string[] = [];
      if (hasTrash) {
        const all = await fetchAllSessions();
        if (!all) return;
        inventory.push(...all.sessions.map((s) => s.id));
      }
      const ids = new Set([...latest.current.map((s) => s.id), ...inventory]);
      sweepOrphanDrafts(ids);
      sweepOrphanComments(ids);
    })();
    /* eslint-enable react-you-might-not-need-an-effect/no-event-handler */
  }, [sessionsLoaded, sessions, trashedCount]);
}
