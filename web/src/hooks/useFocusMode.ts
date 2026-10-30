import { useCallback } from "react";
import { useSearchParams } from "react-router-dom";
import { focusSessionPath, replaceDocument } from "../lib/sessionRoute";

export interface FocusMode {
  /** The only session this window shows, or null outside focus mode. */
  focusSessionId: string | null;
  /** Turns this window into a focus window on `sessionId`. */
  enterFocusMode: (sessionId: string) => void;
  exitFocusMode: () => void;
}

const enterFocusMode = (sessionId: string): void => replaceDocument(focusSessionPath(sessionId));

/** Focus mode for this window: on while the route is `/session/<id>?focus=1` (see `focusSessionPath`), so a reload
 *  keeps it. The URL is the only state: storage would be copied into the windows this one opens. `routedSessionId`
 *  is the session of the current `/session/<id>` route. Entering and exiting load a new document in place of this
 *  history entry: the service worker reads a window's mode from the URL its document loaded with, and Back does not
 *  toggle the mode. */
export function useFocusMode(routedSessionId: string | null): FocusMode {
  const [searchParams] = useSearchParams();
  const focusSessionId = routedSessionId && searchParams.get("focus") === "1" ? routedSessionId : null;
  const exitFocusMode = useCallback(() => {
    if (focusSessionId) replaceDocument(`/session/${encodeURIComponent(focusSessionId)}`);
  }, [focusSessionId]);
  return { focusSessionId, enterFocusMode, exitFocusMode };
}
