import type { ActivityRow } from "./acpTypes";

// Recent rows painted on open; older rows stay in state until "Load earlier".
export const DEFAULT_HISTORY_WINDOW = 150;

export const HISTORY_WINDOW_STEP = 150;

/** User turns anchor the window's top so it never opens mid-turn. */
function isUserTurnBoundary(row: ActivityRow): boolean {
  return row.kind === "user_prompt" || row.kind === "user_diff_comments";
}

export function lastUserBoundaryIndex(rows: readonly ActivityRow[]): number {
  for (let i = rows.length - 1; i >= 0; i -= 1) {
    if (isUserTurnBoundary(rows[i]!)) return i;
  }
  return -1;
}

/** The default window, widened to include the whole last turn so a tool-heavy turn never opens cut off. */
export function initialHistoryWindow(
  rows: readonly ActivityRow[],
  defaultWindow: number = DEFAULT_HISTORY_WINDOW,
): number {
  const boundary = lastUserBoundaryIndex(rows);
  if (boundary < 0) return defaultWindow;
  return Math.max(defaultWindow, rows.length - boundary);
}

/** Pull `start` back to the top-level `Task` so sub-agent children never render as orphan cards, even if that exceeds the cap. */
function snapBackToSubagentParent(rows: readonly ActivityRow[], start: number): number {
  let s = start;
  while (s > 0) {
    const parentId = rows[s]?.tool?.parent_tool_call_id;
    if (!parentId) break;
    let parentIdx = -1;
    for (let i = s - 1; i >= 0; i -= 1) {
      if (rows[i]!.kind === "tool_start" && rows[i]!.tool?.id === parentId) {
        parentIdx = i;
        break;
      }
    }
    if (parentIdx < 0) break;
    s = parentIdx;
  }
  return s;
}

/** Render start for `visibleRows`: never below the cap, snapped forward to a user turn, then back to a sub-agent parent. 0 when everything fits. */
export function historyWindowStart(rows: readonly ActivityRow[], visibleRows: number): number {
  if (visibleRows <= 0) return 0;
  const cap = Math.max(0, rows.length - visibleRows);
  if (cap === 0) return 0;
  let start = cap;
  for (let i = cap; i < rows.length; i += 1) {
    if (isUserTurnBoundary(rows[i]!)) {
      start = i;
      break;
    }
  }
  return snapBackToSubagentParent(rows, start);
}

/** The fold pins to the last `/clear`; the message tree, runtime key, and banner all measure from here. */
export function lastClearIndex(rows: readonly ActivityRow[]): number {
  for (let i = rows.length - 1; i >= 0; i -= 1) {
    if (rows[i]!.kind === "session_cleared") return i;
  }
  return -1;
}

export interface HistoryWindow {
  start: number;
  /** False when only pre-`/clear` rows remain, since the banner reveals those. */
  canLoadEarlier: boolean;
}

export function historyWindow(
  rows: readonly ActivityRow[],
  visibleRows: number,
  showClearedTurns: boolean,
): HistoryWindow {
  const start = historyWindowStart(rows, visibleRows);
  return { start, canLoadEarlier: canLoadEarlierFrom(rows, start, showClearedTurns) };
}

export function canLoadEarlierFrom(rows: readonly ActivityRow[], start: number, showClearedTurns: boolean): boolean {
  const clearIndex = showClearedTurns ? -1 : lastClearIndex(rows);
  return clearIndex < 0 ? start > 0 : start > clearIndex;
}
