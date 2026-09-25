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

export const NAVIGATE_EVENT = "aoe-navigate";

/** `path` must already be same-origin relative (see `isInternalHref`/`toInternalPath`). */
export function requestNavigate(path: string): void {
  if (typeof window === "undefined") return;
  window.dispatchEvent(new CustomEvent(NAVIGATE_EVENT, { detail: { path } }));
}
