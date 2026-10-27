import { useCallback, useMemo, useState } from "react";

import type { ActivityRow } from "../lib/acpTypes";
import {
  DEFAULT_HISTORY_WINDOW,
  HISTORY_WINDOW_STEP,
  canLoadEarlierFrom,
  historyWindow,
  initialHistoryWindow,
} from "../lib/acpHistoryWindow";

export interface HistoryWindowState {
  windowedActivity: ActivityRow[];
  canLoadEarlier: boolean;
  loadEarlier: () => void;
}

export function useHistoryWindow(
  sessionId: string,
  activity: ActivityRow[],
  showClearedTurns: boolean,
  defaultWindow: number = DEFAULT_HISTORY_WINDOW,
): HistoryWindowState {
  const [visibleRows, setVisibleRows] = useState(defaultWindow);
  const [windowPreference, setWindowPreference] = useState(defaultWindow);
  const [windowSessionId, setWindowSessionId] = useState(sessionId);
  // null until first populate, so the initial page lands as the default window, not as growth.
  const [anchorLen, setAnchorLen] = useState<number | null>(null);
  const [anchorRowId, setAnchorRowId] = useState<string | null>(null);
  if (windowSessionId !== sessionId) {
    setWindowSessionId(sessionId);
    setWindowPreference(defaultWindow);
    setVisibleRows(initialHistoryWindow(activity, defaultWindow));
    setAnchorLen(activity.length > 0 ? activity.length : null);
    setAnchorRowId(null);
  } else {
    const preferenceIncreased = defaultWindow > windowPreference;
    if (defaultWindow !== windowPreference) setWindowPreference(defaultWindow);
    if (anchorLen === null) {
      if (activity.length > 0) {
        setAnchorLen(activity.length);
        setVisibleRows(initialHistoryWindow(activity, defaultWindow));
      }
    } else {
      let nextVisible = visibleRows + Math.max(0, activity.length - anchorLen);
      if (preferenceIncreased) {
        nextVisible = Math.max(nextVisible, initialHistoryWindow(activity, defaultWindow));
      }
      if (nextVisible !== visibleRows) setVisibleRows(nextVisible);
      if (activity.length !== anchorLen) setAnchorLen(activity.length);
    }
  }
  const computed = useMemo(
    () => historyWindow(activity, visibleRows, showClearedTurns),
    [activity, visibleRows, showClearedTurns],
  );
  // The start may only move earlier; moving it later shrinks a rendered message (#3707).
  const start = pinnedWindowStart(activity, computed.start, anchorRowId);
  const startRowId = start < activity.length ? (activity[start]?.id ?? null) : null;
  if (startRowId !== anchorRowId && windowSessionId === sessionId) {
    setAnchorRowId(startRowId);
  }
  const canLoadEarlier = canLoadEarlierFrom(activity, start, showClearedTurns);
  const windowedActivity = useMemo(() => (start === 0 ? activity : activity.slice(start)), [activity, start]);
  const loadEarlier = useCallback(() => setVisibleRows((v) => v + HISTORY_WINDOW_STEP), []);
  return { windowedActivity, canLoadEarlier, loadEarlier };
}

export function pinnedWindowStart(rows: readonly ActivityRow[], computed: number, anchorRowId: string | null): number {
  if (anchorRowId === null) return computed;
  const pinned = rows.findIndex((r) => r.id === anchorRowId);
  if (pinned < 0) return computed;
  return Math.min(computed, pinned);
}
