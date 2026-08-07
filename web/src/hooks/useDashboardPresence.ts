import { useEffect } from "react";

const PRESENCE_INTERVAL_MS = 10_000;

function isForeground(): boolean {
  return document.visibilityState === "visible" && document.hasFocus();
}

/**
 * Report only genuine foreground dashboard use to the server's push
 * suppression logic. Session polling continues in a backgrounded browser,
 * but that traffic must not make a phone miss a notification.
 */
export function useDashboardPresence(): void {
  useEffect(() => {
    const report = (active: boolean, keepalive = false) => {
      void fetch("/api/presence", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ active }),
        keepalive,
      }).catch(() => {
        // Presence is best effort. A failed heartbeat naturally expires.
      });
    };
    const update = () => {
      const active = isForeground();
      report(active, !active);
    };
    const clear = () => report(false, true);

    update();
    const interval = window.setInterval(update, PRESENCE_INTERVAL_MS);
    document.addEventListener("visibilitychange", update);
    window.addEventListener("focus", update);
    window.addEventListener("blur", clear);
    window.addEventListener("pageshow", update);
    window.addEventListener("pagehide", clear);

    return () => {
      window.clearInterval(interval);
      document.removeEventListener("visibilitychange", update);
      window.removeEventListener("focus", update);
      window.removeEventListener("blur", clear);
      window.removeEventListener("pageshow", update);
      window.removeEventListener("pagehide", clear);
      clear();
    };
  }, []);
}
