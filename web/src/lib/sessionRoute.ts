// Custom event dispatched when an in-app toast (for a focused PWA
// client) is tapped. App listens and navigates to the session.

export const OPEN_SESSION_EVENT = "aoe-open-session";

export function requestOpenSession(sessionId: string): void {
  if (typeof window === "undefined") return;
  window.dispatchEvent(new CustomEvent(OPEN_SESSION_EVENT, { detail: { sessionId } }));
}
