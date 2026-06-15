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

/**
 * Index into `rows` from which the transcript should render, given how
 * many recent rows the caller wants visible (`visibleRows`).
 *
 * The hard cap is primary: the result is never below
 * `rows.length - visibleRows`, so a single huge agent turn (hundreds of
 * tool rows under one prompt) can never blow the window past the cap.
 * Within that cap the start snaps FORWARD to the nearest user turn
 * boundary so the top is a clean user message rather than a half turn.
 * When no boundary sits at or after the cap cut, the cut is used as-is.
 *
 * Returns 0 when every row fits (nothing earlier to load).
 */
export function historyWindowStart(rows: readonly ActivityRow[], visibleRows: number): number {
  if (visibleRows <= 0) return 0;
  const cap = Math.max(0, rows.length - visibleRows);
  if (cap === 0) return 0;
  for (let i = cap; i < rows.length; i += 1) {
    if (isUserTurnBoundary(rows[i]!)) return i;
  }
  return cap;
}
