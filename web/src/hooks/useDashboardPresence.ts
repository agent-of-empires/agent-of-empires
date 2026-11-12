import { useEffect } from "react";
import { listen } from "./domEvents";

const PRESENCE_INTERVAL_MS = 10_000;

// Keys this page's presence apart from other windows of the same browser; not a secret.
const WINDOW_ID = Math.random().toString(36).slice(2);

function isForeground(): boolean {
  return document.visibilityState === "visible" && document.hasFocus();
}

function report(active: boolean, focusSessionId: string | null, keepalive: boolean): void {
  void fetch("/api/presence", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ active, window: WINDOW_ID, focus_session: focusSessionId }),
    keepalive,
  }).catch(() => {});
}

/** Reports this window's foreground state, which suppresses push notifications while the user is looking. A
 *  focus-mode window passes `focusSessionId`, so it only suppresses notifications about that session. */
export function useDashboardPresence(focusSessionId: string | null): void {
  useEffect(() => {
    const update = () => {
      const active = isForeground();
      report(active, focusSessionId, !active);
    };
    const clear = () => report(false, focusSessionId, true);

    update();
    const interval = window.setInterval(update, PRESENCE_INTERVAL_MS);
    const stopUpdate = listen(update, [document, "visibilitychange"], [window, "focus"], [window, "pageshow"]);
    const stopClear = listen(clear, [window, "blur"], [window, "pagehide"]);

    return () => {
      window.clearInterval(interval);
      stopUpdate();
      stopClear();
    };
  }, [focusSessionId]);

  // Cleared on unmount only. A focus change re-reports under the same window id, which replaces the server's entry;
  // clearing first could land after the new report and drop this window until the next heartbeat.
  useEffect(() => () => report(false, null, true), []);
}
