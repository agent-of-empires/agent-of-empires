import { useMemo } from "react";
import type { SessionResponse } from "../lib/types";
import { countUnreadSessions, countWaitingSessions } from "../lib/session";
import { useUnreadIndicatorEnabled } from "../lib/unreadIndicator";

/** Aggregate counts for the sidebar-toggle badges in TopBar, so the special states are visible even while the
 *  sidebar itself is collapsed or scrolled past. */
export function useAttentionCounts(
  sessions: readonly SessionResponse[],
  activeSessionId: string | null,
): { unreadCount: number; waitingCount: number } {
  const unreadIndicatorEnabled = useUnreadIndicatorEnabled();
  const unreadCount = useMemo(
    () => countUnreadSessions(sessions, activeSessionId, unreadIndicatorEnabled),
    [sessions, activeSessionId, unreadIndicatorEnabled],
  );
  const waitingCount = useMemo(() => countWaitingSessions(sessions), [sessions]);
  return { unreadCount, waitingCount };
}
