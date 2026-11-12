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

/** Where a navigation this window asked for goes: in place, into a new window, or in place within focus mode because
 *  it targets the focused session (see `withFocusMode`). `sessionId` is set when the target is a session route. */
export type FocusNavigation = "here" | "new-window" | "focused";

export function focusNavigation(focusSessionId: string | null, sessionId?: string): FocusNavigation {
  if (!focusSessionId) return "here";
  return sessionId === focusSessionId ? "focused" : "new-window";
}

/** `path` (relative, same origin) with `focus=1` set, so that following it keeps the window in focus mode. */
export function withFocusMode(path: string): string {
  const url = new URL(path, "http://aoe.invalid");
  url.searchParams.set("focus", "1");
  return `${url.pathname}${url.search}${url.hash}`;
}

/** The focus session a URL names (`/session/<id>?focus=1`), or null. The path is read exactly as the router's
 *  `useMatch("/session/:sessionId")` reads it (case-insensitive, trailing slashes, percent-decoded segments), so the
 *  document and the route always agree on the mode. `public/sw.js` `focusSessionOf` reads client URLs the same way. */
export function focusSessionOfUrl(url: string): string | null {
  let parsed: URL;
  try {
    parsed = new URL(url, "http://aoe.invalid");
  } catch {
    return null;
  }
  return parsed.searchParams.get("focus") === "1" ? routedSessionOfPath(parsed.pathname) : null;
}

/** The `sessionId` param react-router 7 matches in `pathname` for `/session/:sessionId`, or null. */
function routedSessionOfPath(pathname: string): string | null {
  let decoded: string;
  try {
    decoded = pathname
      .split("/")
      .map((segment) => decodeURIComponent(segment).replace(/\//g, "%2F"))
      .join("/");
  } catch {
    decoded = pathname;
  }
  const match = /^\/session\/([^/]+)\/*$/i.exec(decoded);
  return match ? match[1]!.replace(/%2F/g, "/") : null;
}

/** The name a focus window shows for its session, in its tab title, note and gone screen. */
export function focusSessionTitle(session: { title: string; branch?: string | null }): string {
  return session.title.trim() || session.branch || "Untitled session";
}
