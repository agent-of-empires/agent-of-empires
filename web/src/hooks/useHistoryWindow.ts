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
 *
 * The window grows by however many rows are added after first paint
 * (live turns appended at the tail, or an older page prepended at the
 * head) so rows already on screen stay put. Without this the end-anchored
 * `length - visibleRows` start would slide forward on every new turn and
 * fold visible older rows back behind "Load earlier". See #2236.
 */
export function useHistoryWindow(
  sessionId: string,
  activity: ActivityRow[],
  showClearedTurns: boolean,
): HistoryWindowState {
  const [visibleRows, setVisibleRows] = useState(DEFAULT_HISTORY_WINDOW);
  const [windowSessionId, setWindowSessionId] = useState(sessionId);
  // Row count the window was last reconciled against. `null` until the
  // transcript first populates, so the initial recent-first load lands as
  // the DEFAULT window rather than being treated as growth (which would
  // render the whole first page instead of just its tail).
  const [anchorLen, setAnchorLen] = useState<number | null>(null);
  if (windowSessionId !== sessionId) {
    setWindowSessionId(sessionId);
    setVisibleRows(DEFAULT_HISTORY_WINDOW);
    setAnchorLen(activity.length > 0 ? activity.length : null);
  } else if (anchorLen === null) {
    if (activity.length > 0) setAnchorLen(activity.length);
  } else if (activity.length !== anchorLen) {
    // Grow the window by exactly the rows added so the start index holds
    // and on-screen rows don't fold. A shrink (retention trim) just
    // resyncs the anchor; the smaller set renders whole.
    if (activity.length > anchorLen) {
      setVisibleRows((v) => v + (activity.length - anchorLen));
    }
    setAnchorLen(activity.length);
  }
  const { start, canLoadEarlier } = useMemo(
    () => historyWindow(activity, visibleRows, showClearedTurns),
    [activity, visibleRows, showClearedTurns],
  );
  const windowedActivity = useMemo(() => (start === 0 ? activity : activity.slice(start)), [activity, start]);
  const loadEarlier = useCallback(() => setVisibleRows((v) => v + HISTORY_WINDOW_STEP), []);
  return { windowedActivity, canLoadEarlier, loadEarlier };
}
