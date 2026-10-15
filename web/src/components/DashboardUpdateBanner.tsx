import { useEffect, useRef, useState } from "react";
import { fetchAbout } from "../lib/api";
import { currentWebBuildId, isWebUpdateAvailable } from "../lib/webBuildId";

// Re-check cadence floor. Visibility flips can arrive in bursts (iOS
// fires several when a PWA resumes); one /api/about per window is plenty.
const CHECK_THROTTLE_MS = 30_000;

/** "Dashboard updated, reload" banner. */
export function DashboardUpdateBanner() {
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
    return () => {
      cancelled = true;
      document.removeEventListener("visibilitychange", onVisibility);
      window.removeEventListener("online", onOnline);
      window.removeEventListener("vite:preloadError", onPreloadError);
    };
  }, []);

  if (!updateAvailable) return null;

  return (
    <div
      role="status"
      aria-label="Dashboard update available"
      className="bg-brand-600/10 border-b border-brand-600/30 px-4 py-2 flex items-center justify-center gap-3 text-xs font-mono text-brand-300 animate-fade-in"
    >
      <span className="w-1.5 h-1.5 rounded-full bg-brand-400 shrink-0" />
      <span>Dashboard updated.</span>
      <button
        type="button"
        onClick={() => window.location.reload()}
        className="underline hover:text-brand-200 cursor-pointer"
      >
        Reload now
      </button>
    </div>
  );
}
