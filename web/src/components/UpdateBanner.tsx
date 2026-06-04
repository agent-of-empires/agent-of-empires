import { useEffect, useState } from "react";
import { fetchUpdateStatus } from "../lib/api";
import type { UpdateStatus } from "../lib/api";
import { safeGetItem, safeSetItem } from "../lib/safeStorage";

const DISMISS_KEY = "aoe-update-dismissed-version";

// Minimum poll period regardless of what the server reports. Guards
// against a misconfigured `web_poll_interval_minutes = 0` hammering
// `/api/system/update-status` (and through it the GitHub API once the
// server-side cache lapses).
const MIN_POLL_MINUTES = 5;

function readDismissed(): string | null {
  return safeGetItem(DISMISS_KEY);
}

function writeDismissed(version: string) {
  safeSetItem(DISMISS_KEY, version);
}

/**
 * Top-of-app banner shown when `update_available` is true. Dismiss
 * persists by latest_version, so a newer release re-surfaces it.
 * Polls on mount + at `web_poll_interval_minutes` cadence + on tab
 * visibilitychange. Honors `update_check_mode`: server returns
 * `update_available: false` when mode = off, so nothing renders.
 * Mode = auto also suppresses the banner (the runtime installs
 * silently and the user picks the new binary up next launch).
 * See #984 and #1140.
 */
export function UpdateBanner() {
  const [status, setStatus] = useState<UpdateStatus | null>(null);
  const [dismissedVersion, setDismissedVersion] = useState<string | null>(
    () => readDismissed(),
  );

  useEffect(() => {
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | null = null;

    const poll = async () => {
      const s = await fetchUpdateStatus();
      if (cancelled) return;
      if (s) setStatus(s);
      const minutes = Math.max(
        MIN_POLL_MINUTES,
        s?.web_poll_interval_minutes ?? 60,
      );
      timer = setTimeout(poll, minutes * 60_000);
    };

    const initialTimer = setTimeout(() => void poll(), 0);

    const onVisibility = () => {
      if (document.visibilityState === "visible") {
        if (timer) clearTimeout(timer);
        poll();
      }
    };
    document.addEventListener("visibilitychange", onVisibility);

    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
      clearTimeout(initialTimer);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, []);

  if (!status || !status.update_available || !status.latest_version) {
    return null;
  }
  // Suppress the banner in auto mode (the runtime is handling the install
  // in the background; nothing for the user to do).
  if (status.update_check_mode === "auto") return null;
  if (dismissedVersion === status.latest_version) return null;

  const onDismiss = () => {
    if (!status.latest_version) return;
    writeDismissed(status.latest_version);
    setDismissedVersion(status.latest_version);
  };

  return (
    <div
      role="status"
      aria-label={`Update available: v${status.latest_version}`}
      className="bg-brand-600/10 border-b border-brand-600/30 px-4 py-2 flex items-center justify-center gap-3 text-xs font-mono text-brand-300 animate-fade-in"
    >
      <span className="w-1.5 h-1.5 rounded-full bg-brand-400 shrink-0" />
      <span>
        Update available: v{status.current_version} → v{status.latest_version}.
      </span>
      {status.release_url && (
        <a
          href={status.release_url}
          target="_blank"
          rel="noopener noreferrer"
          className="underline hover:text-brand-200"
        >
          Release notes
        </a>
      )}
      <button
        type="button"
        onClick={onDismiss}
        aria-label="Dismiss update notice"
        className="ml-2 text-text-muted hover:text-text-secondary cursor-pointer text-base leading-none px-1"
      >
        &times;
      </button>
    </div>
  );
}
