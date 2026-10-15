import { useEffect } from "react";
import { useLocation, useNavigate } from "react-router-dom";
import { isStandalone } from "../lib/platform";
import { safeGetItem, safeRemoveItem, safeSetItem } from "../lib/safeStorage";

export const LAST_SESSION_KEY = "aoe-last-session-id";

// Restore only in a standalone PWA: every fresh page load has `location.key === "default"`.
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
    /* eslint-disable react-you-might-not-need-an-effect/no-event-handler */
    if (
      location.key !== "default" ||
      activeSessionId ||
      location.pathname !== "/" ||
      !sessionsLoaded ||
      !isStandalone()
    ) {
      return;
    }
    /* eslint-enable react-you-might-not-need-an-effect/no-event-handler */
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
