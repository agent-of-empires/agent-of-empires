import { useEffect, useRef, useState } from "react";
import { fetchAbout } from "../lib/api";
import { onServerDownChange } from "../lib/connectionState";
import { currentWebBuildId, isWebUpdateAvailable } from "../lib/webBuildId";

// Re-check cadence floor. Visibility flips can arrive in bursts (iOS
// fires several when a PWA resumes); one /api/about per window is plenty.
const CHECK_THROTTLE_MS = 30_000;

/** Whether the Daemon now serves a newer dashboard bundle than this page loaded. */
export function useDashboardUpdate(): boolean {
  const [updateAvailable, setUpdateAvailable] = useState(false);
  const lastCheckRef = useRef(0);

  useEffect(() => {
    const ownId = currentWebBuildId();
    // Vite dev server (no hashed entry): nothing to compare.
    if (!ownId) return;
    let cancelled = false;

    const check = async (force: boolean) => {
      const now = Date.now();
      if (!force && now - lastCheckRef.current < CHECK_THROTTLE_MS) return;
      lastCheckRef.current = now;
      const about = await fetchAbout();
      if (cancelled || !about) return;
      if (isWebUpdateAvailable(ownId, about.web_build_id)) {
        setUpdateAvailable(true);
      }
    };

    const onVisibility = () => {
      if (document.visibilityState === "visible") void check(false);
    };
    const onOnline = () => void check(false);
    // A failed dynamic import means the chunk this page wants no longer exists on the server: the bundle changed
    // underneath us.
    const onPreloadError = () => void check(true);

    void check(true);
    document.addEventListener("visibilitychange", onVisibility);
    window.addEventListener("online", onOnline);
    window.addEventListener("vite:preloadError", onPreloadError);
    // A restarted Daemon may serve a new bundle, and the reconnect can land inside the throttle window.
    const offServerDown = onServerDownChange((down) => {
      if (!down) void check(true);
    });
    return () => {
      cancelled = true;
      document.removeEventListener("visibilitychange", onVisibility);
      window.removeEventListener("online", onOnline);
      window.removeEventListener("vite:preloadError", onPreloadError);
      offServerDown();
    };
  }, []);

  return updateAvailable;
}
