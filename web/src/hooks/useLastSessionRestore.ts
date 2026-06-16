import { useEffect } from "react";
import { useLocation, useNavigate } from "react-router-dom";
import { safeGetItem, safeRemoveItem, safeSetItem } from "../lib/safeStorage";

// Device-local id of the last session the user had open, so an installed PWA
// reopens to it instead of the dashboard (#2103). Not registered in webUiSync:
// sessions/worktrees are host-specific, so syncing this across devices would
// redirect to ids that don't exist locally.
export const LAST_SESSION_KEY = "aoe-last-session-id";

/**
 * Remember the active session and restore it on a PWA relaunch (#2103).
 *
 * An installed PWA reopens at its install URL "/", so it always landed on the
 * dashboard instead of the session the user last had open. This persists the
 * active session id whenever it changes and, on a cold launch that lands on the
 * dashboard root, redirects to it once sessions have loaded.
 *
 * Restore is scoped to the initial history entry (`location.key === "default"`)
 * so an in-app navigation to the dashboard never bounces the user back into a
 * session, and the persist effect only clears the key on such an in-app return,
 * never on the initial entry, so the restore below still sees it on a cold
 * launch. A stored id that no longer matches a loaded session is dropped.
 */
export function useLastSessionRestore(params: {
  activeSessionId: string | null;
  sessions: readonly { id: string }[];
  sessionsLoaded: boolean;
}): void {
  const { activeSessionId, sessions, sessionsLoaded } = params;
  const navigate = useNavigate();
  const location = useLocation();

  useEffect(() => {
    if (activeSessionId) {
      safeSetItem(LAST_SESSION_KEY, activeSessionId);
    } else if (location.pathname === "/" && location.key !== "default") {
      safeRemoveItem(LAST_SESSION_KEY);
    }
  }, [activeSessionId, location.pathname, location.key]);

  useEffect(() => {
    // Restore reacts to router state (cold-launch URL, back/forward, deep links,
    // push-driven navigation), not a single DOM event, so it must be an effect.
    // The rule misreads the hook's params as component props and the navigate as
    // passing data to a parent; neither applies to a router-state reactor.
    // eslint-disable-next-line react-you-might-not-need-an-effect/no-event-handler
    if (location.key !== "default" || activeSessionId || location.pathname !== "/" || !sessionsLoaded) {
      return;
    }
    const saved = safeGetItem(LAST_SESSION_KEY);
    if (!saved) return;
    // eslint-disable-next-line react-you-might-not-need-an-effect/no-pass-data-to-parent
    if (sessions.some((s) => s.id === saved)) {
      navigate(`/session/${encodeURIComponent(saved)}`, { replace: true });
    } else {
      safeRemoveItem(LAST_SESSION_KEY);
    }
  }, [location.key, location.pathname, activeSessionId, sessionsLoaded, sessions, navigate]);
}
