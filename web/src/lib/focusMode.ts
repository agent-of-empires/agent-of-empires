/** Rules for a focus-mode window, which shows a single session (see `useFocusMode`). */

/** The sessions whose attention badges and sounds this window reports: all of them, or only the focused one. */
export function noticedSessions<T extends { id: string }>(
  sessions: readonly T[],
  focusSessionId: string | null,
): readonly T[] {
  return focusSessionId ? sessions.filter((s) => s.id === focusSessionId) : sessions;
}

/** Where a navigation this window asked for goes: in place, into a new window, or nowhere because the focused
 *  session is already shown. `sessionId` is set when the target is a session route. */
export type FocusNavigation = "here" | "new-window" | "none";

export function focusNavigation(focusSessionId: string | null, sessionId?: string): FocusNavigation {
  if (!focusSessionId) return "here";
  return sessionId === focusSessionId ? "none" : "new-window";
}
