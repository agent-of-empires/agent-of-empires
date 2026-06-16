import { useCallback, useMemo, useState } from "react";

import type { ActivityRow } from "../lib/acpTypes";
import { DEFAULT_HISTORY_WINDOW, HISTORY_WINDOW_STEP, historyWindow } from "../lib/acpHistoryWindow";

export interface HistoryWindowState {
  /** The recent slice of `activity` to render. */
  windowedActivity: ActivityRow[];
  /** True when older rows remain that "Load earlier" would reveal. */
  canLoadEarlier: boolean;
  /** Reveal an additional chunk of older history. */
  loadEarlier: () => void;
}

/**
 * Window the structured-view transcript to its most recent rows so a
 * long session does not block first paint, growing on demand via
 * `loadEarlier`. The visible window resets to recent whenever
 * `sessionId` changes (adjust-state-on-prop-change, no effect, per the
 * react-you-might-not-need-an-effect lint). See #2144.
 */
export function useHistoryWindow(
  sessionId: string,
  activity: ActivityRow[],
  showClearedTurns: boolean,
): HistoryWindowState {
  const [visibleRows, setVisibleRows] = useState(DEFAULT_HISTORY_WINDOW);
  const [windowSessionId, setWindowSessionId] = useState(sessionId);
  if (windowSessionId !== sessionId) {
    setWindowSessionId(sessionId);
    setVisibleRows(DEFAULT_HISTORY_WINDOW);
  }
  const { start, canLoadEarlier } = useMemo(
    () => historyWindow(activity, visibleRows, showClearedTurns),
    [activity, visibleRows, showClearedTurns],
  );
  const windowedActivity = useMemo(() => (start === 0 ? activity : activity.slice(start)), [activity, start]);
  const loadEarlier = useCallback(() => setVisibleRows((v) => v + HISTORY_WINDOW_STEP), []);
  return { windowedActivity, canLoadEarlier, loadEarlier };
}
