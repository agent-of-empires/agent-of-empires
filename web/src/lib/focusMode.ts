/** Rules for a focus-mode window, which shows a single session (see `useFocusMode`). */

import { toastBus, type ToastApi } from "./toastBus";

/** Where the outcome of a background session create is reported: the app's toasts, or nowhere in a focus window
 *  (`quiet`), which only notices its own session. */
export function createToasts(quiet: boolean): ToastApi | null {
  return quiet ? null : toastBus.handler;
}

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

/** The focus session a URL names (`/session/<id>?focus=1`, optional trailing slash), or null. `public/sw.js`
 *  `focusSessionOf` reads client URLs the same way. */
export function focusSessionOfUrl(url: string): string | null {
  try {
    const parsed = new URL(url, "http://aoe.invalid");
    const match = /^\/session\/([^/]+)\/?$/.exec(parsed.pathname);
    return match && parsed.searchParams.get("focus") === "1" ? decodeURIComponent(match[1]!) : null;
  } catch {
    return null;
  }
}

/** The name a focus window shows for its session, in its tab title, note and gone screen. */
export function focusSessionTitle(session: { title: string; branch?: string | null }): string {
  return session.title.trim() || session.branch || "Untitled session";
}
