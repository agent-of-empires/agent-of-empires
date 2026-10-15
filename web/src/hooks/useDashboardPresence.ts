import { useEffect } from "react";
import { listen } from "./domEvents";

const PRESENCE_INTERVAL_MS = 10_000;

function isForeground(): boolean {
  return document.visibilityState === "visible" && document.hasFocus();
}

export function useDashboardPresence(): void {
  useEffect(() => {
    const report = (active: boolean, keepalive = false) => {
      void fetch("/api/presence", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ active }),
        keepalive,
      }).catch(() => {});
    };
    const update = () => {
      const active = isForeground();
      report(active, !active);
    };
    const clear = () => report(false, true);

    update();
    const interval = window.setInterval(update, PRESENCE_INTERVAL_MS);
    const stopUpdate = listen(update, [document, "visibilitychange"], [window, "focus"], [window, "pageshow"]);
    const stopClear = listen(clear, [window, "blur"], [window, "pagehide"]);

    return () => {
      window.clearInterval(interval);
      stopUpdate();
      stopClear();
      clear();
    };
  }, []);
}
