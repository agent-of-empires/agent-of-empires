// Custom events for navigation requested from outside the router's render
// tree (toasts, plugin links). App listens on both and calls its router's
// `navigate`.

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

export const NAVIGATE_EVENT = "aoe-navigate";

/** `path` must already be same-origin relative (see `isInternalHref`/`toInternalPath`). */
export function requestNavigate(path: string): void {
  if (typeof window === "undefined") return;
  window.dispatchEvent(new CustomEvent(NAVIGATE_EVENT, { detail: { path } }));
}
