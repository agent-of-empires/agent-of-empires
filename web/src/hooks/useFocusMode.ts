import { useCallback, useEffect, useState } from "react";
import { useLocation, useSearchParams } from "react-router-dom";
import { focusSessionTitle } from "../lib/focusMode";
import { enterFocusWindow, loadedFocusSessionId, replaceDocument } from "../lib/sessionRoute";

export interface FocusMode {
  /** The only session this window shows, or null outside focus mode. */
  focusSessionId: string | null;
  /** Turns this window into a focus window on `sessionId`. */
  enterFocusMode: (sessionId: string) => void;
  exitFocusMode: () => void;
}

/** Focus mode for this window: on while the route is `/session/<id>?focus=1` (see `focusSessionPath`), so a reload
 *  keeps it. The URL is the only state: storage would be copied into the windows this one opens. `routedSessionId`
 *  is the session of the current `/session/<id>` route. Entering and exiting load a new document in place of this
 *  history entry: the service worker reads a window's mode from the URL its document loaded with, and Back does not
 *  toggle the mode. A router navigation that changes the mode anyway (an in-app link to a `?focus=1` URL, or Back
 *  into one) reloads the document at the new URL. */
export function useFocusMode(routedSessionId: string | null): FocusMode {
  const [searchParams] = useSearchParams();
  const location = useLocation();
  const focusSessionId = routedSessionId && searchParams.get("focus") === "1" ? routedSessionId : null;
  // Synchronizes the document with the route whatever navigated it (links, toasts, Back), not one event's handler.
  useEffect(() => {
    // eslint-disable-next-line react-you-might-not-need-an-effect/no-event-handler
    if (focusSessionId !== loadedFocusSessionId()) {
      replaceDocument(`${location.pathname}${location.search}${location.hash}`);
    }
  }, [focusSessionId, location]);
  const exitFocusMode = useCallback(() => {
    if (focusSessionId) replaceDocument(`/session/${encodeURIComponent(focusSessionId)}`);
  }, [focusSessionId]);
  return { focusSessionId, enterFocusMode: enterFocusWindow, exitFocusMode };
}

export interface FocusSessionState {
  /** The session's name, kept from its last sighting once it is deleted; null outside focus mode or when this
   *  window never saw it. */
  title: string | null;
  /** Why the session can no longer show here, or null while it can. */
  gone: "trashed" | "deleted" | null;
}

/** Tracks the session of a focus window (`session`, or undefined once it is deleted or still loading) and titles
 *  the browser tab after it, so the window is easy to find among tabs. Inactive when `focusSessionId` is null. */
export function useFocusSession(
  focusSessionId: string | null,
  session: { title: string; branch?: string | null; trashed_at?: string | null } | undefined,
  sessionsLoaded: boolean,
): FocusSessionState {
  const current = focusSessionId && session ? focusSessionTitle(session) : null;
  const [lastTitle, setLastTitle] = useState<string | null>(null);
  if (current !== null && current !== lastTitle) setLastTitle(current);
  const title = focusSessionId ? (current ?? lastTitle) : null;

  useEffect(() => {
    if (title === null) return;
    const previous = document.title;
    document.title = title;
    return () => {
      document.title = previous;
    };
  }, [title]);

  let gone: FocusSessionState["gone"] = null;
  if (focusSessionId && sessionsLoaded) {
    if (!session) gone = "deleted";
    else if (session.trashed_at) gone = "trashed";
  }
  return { title, gone };
}
