import type { ActivityRow } from "./acpTypes";

/** Recent activity rows the structured view paints on open. Older rows
 *  stay in reducer state but are not rendered until the user loads them,
 *  so a long transcript no longer blocks first paint on mobile. The
 *  whole-history fetch + reduce still happens (proposal A in #2144); the
 *  multi-page network cost on very large sessions is a separate
 *  follow-up. See #2144. */
export const DEFAULT_HISTORY_WINDOW = 150;

/** Extra rows revealed per "Load earlier" activation. */
export const HISTORY_WINDOW_STEP = 150;

/** User turns anchor the window's top so it never opens on a dangling
 *  mid-turn assistant fragment. Typed diff-comment turns count too. */
function isUserTurnBoundary(row: ActivityRow): boolean {
  return row.kind === "user_prompt" || row.kind === "user_diff_comments";
}

/** Pull `start` back so the window never opens strictly between a
 *  sub-agent `Task` row and its child tool calls. The child rows carry
 *  `tool.parent_tool_call_id` (the parent Task's `tool.id`); when `start`
 *  lands on such a child whose parent sits earlier, the parent has been
 *  folded into "earlier" and the children render as headless orphan
 *  cards instead of one SubagentCard (#2313). Walk the parent chain back
 *  to the top-level Task. A `user_prompt` boundary start carries no
 *  `tool`, so the loop is a no-op there and the forward-snap stands.
 *
 *  ponytail: linear scan per hop, bounded by the sub-agent block, only
 *  runs at the window cut. A sub-agent with more child rows than the
 *  whole window pulls `start` back past the cap, rendering the full
 *  block; a headless run of orphan cards is the worse outcome. */
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

/**
 * Index into `rows` from which the transcript should render, given how
 * many recent rows the caller wants visible (`visibleRows`).
 *
 * The hard cap is primary: the result is never below
 * `rows.length - visibleRows`, so a single huge agent turn (hundreds of
 * tool rows under one prompt) can never blow the window past the cap.
 * Within that cap the start snaps FORWARD to the nearest user turn
 * boundary so the top is a clean user message rather than a half turn.
 * When no boundary sits at or after the cap cut, the cut is used as-is,
 * then pulled back so it never severs a sub-agent from its children
 * (#2313).
 *
 * Returns 0 when every row fits (nothing earlier to load).
 */
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

/** Index of the latest `/clear` divider, or -1 when cleared turns are
 *  shown (folding off) or there is no clear. Rows before it are hidden
 *  behind the ClearedTurnsBanner, not by the history window. */
function lastClearedIndex(rows: readonly ActivityRow[], showClearedTurns: boolean): number {
  if (showClearedTurns) return -1;
  for (let i = rows.length - 1; i >= 0; i -= 1) {
    if (rows[i]!.kind === "session_cleared") return i;
  }
  return -1;
}

export interface HistoryWindow {
  /** Index to start rendering from; 0 renders the whole transcript. */
  start: number;
  /** Whether older rows remain that "Load earlier" would actually
   *  reveal. False when the only hidden rows are pre-`/clear` (those are
   *  reached via the ClearedTurnsBanner), so the control is not a no-op. */
  canLoadEarlier: boolean;
}

/** Resolve the render window for the structured-view transcript. */
export function historyWindow(
  rows: readonly ActivityRow[],
  visibleRows: number,
  showClearedTurns: boolean,
): HistoryWindow {
  const start = historyWindowStart(rows, visibleRows);
  const clearIndex = lastClearedIndex(rows, showClearedTurns);
  const canLoadEarlier = clearIndex < 0 ? start > 0 : start > clearIndex;
  return { start, canLoadEarlier };
}
