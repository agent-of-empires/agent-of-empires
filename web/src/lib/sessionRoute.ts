// Custom events for navigation requested from outside the router's render
// tree (toasts, plugin links). App listens on both and calls its router's
// `navigate`.

import { focusSessionOfUrl } from "./focusMode";

export const OPEN_SESSION_EVENT = "aoe-open-session";

/** `path`, when given, overrides the default `/session/<id>` route (e.g. a
 *  plugin link's own query string or hash), but must still resolve to `sessionId`'s route. */
export function requestOpenSession(sessionId: string, path?: string): void {
  if (typeof window === "undefined") return;
  window.dispatchEvent(new CustomEvent(OPEN_SESSION_EVENT, { detail: { sessionId, path } }));
}

/** The URL of a focus-mode window. `public/sw.js` parses the same shape from client URLs. */
export function focusSessionPath(sessionId: string): string {
  return `/session/${encodeURIComponent(sessionId)}?focus=1`;
}

/** Opens `sessionId` in focus mode in a new window, leaving this one as it is. */
export function openFocusWindow(sessionId: string): void {
  if (typeof window === "undefined") return;
  window.open(focusSessionPath(sessionId), "_blank");
}

/** Turns this window into a focus window on `sessionId`. */
export function enterFocusWindow(sessionId: string): void {
  replaceDocument(focusSessionPath(sessionId));
}

/** Loads `path` as a new document in place of the current history entry. Unlike a router navigation, this updates
 *  the URL the service worker sees for this window (`Client.url`). */
export function replaceDocument(path: string): void {
  if (typeof window === "undefined") return;
  window.location.replace(path);
}

/** The focus session of the URL this document loaded with, which is the one the service worker sees for the window
 *  (`Client.url`), whatever history changes followed. */
export function loadedFocusSessionId(): string | null {
  if (typeof window === "undefined") return null;
  const [navigation] = performance.getEntriesByType?.("navigation") ?? [];
  return focusSessionOfUrl(navigation?.name || window.location.href);
}

export const NAVIGATE_EVENT = "aoe-navigate";

/** `path` must already be same-origin relative (see `isInternalHref`/`toInternalPath`). */
export function requestNavigate(path: string): void {
  if (typeof window === "undefined") return;
  window.dispatchEvent(new CustomEvent(NAVIGATE_EVENT, { detail: { path } }));
}
