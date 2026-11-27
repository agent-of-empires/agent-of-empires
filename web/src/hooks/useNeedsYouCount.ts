import { useMemo } from "react";
import type { SessionResponse } from "../lib/types";
import { usePluginAttention } from "../lib/pluginUiContext";
import { sessionNeedsAttention } from "../lib/sidebarSort";
import { useUnreadIndicatorEnabled } from "../lib/unreadIndicator";

/** The Sessions the sidebar headers count as needing you, for the sidebar toggle's badge. */
export function useNeedsYouCount(sessions: readonly SessionResponse[]): number {
  const pluginAttention = usePluginAttention();
  const unreadCounts = useUnreadIndicatorEnabled();
  return useMemo(
    () => sessions.filter((s) => sessionNeedsAttention(s, pluginAttention, unreadCounts)).length,
    [sessions, pluginAttention, unreadCounts],
  );
}
