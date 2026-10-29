import { useCallback } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { focusSessionPath } from "../lib/sessionRoute";

export interface FocusMode {
  /** The only session this window shows, or null outside focus mode. */
  focusSessionId: string | null;
  /** Turns this window into a focus window on `sessionId`. */
  enterFocusMode: (sessionId: string) => void;
  exitFocusMode: () => void;
}

/** Focus mode for this window: on while the route is `/session/<id>?focus=1` (see `focusSessionPath`), so a reload
 *  keeps it. The URL is the only state: storage would be copied into the windows this one opens. `routedSessionId`
 *  is the session of the current `/session/<id>` route. Entering and exiting replace the history entry, so Back
 *  does not toggle the mode. */
export function useFocusMode(routedSessionId: string | null): FocusMode {
  const navigate = useNavigate();
  const [searchParams] = useSearchParams();
  const focusSessionId = routedSessionId && searchParams.get("focus") === "1" ? routedSessionId : null;

  const enterFocusMode = useCallback(
    (sessionId: string) => navigate(focusSessionPath(sessionId), { replace: true }),
    [navigate],
  );
  const exitFocusMode = useCallback(() => {
    if (focusSessionId) navigate(`/session/${encodeURIComponent(focusSessionId)}`, { replace: true });
  }, [focusSessionId, navigate]);

  return { focusSessionId, enterFocusMode, exitFocusMode };
}
