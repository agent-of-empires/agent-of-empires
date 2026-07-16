import { useEffect, useState } from "react";
import { dismissUpdate, fetchUpdateStatus } from "../lib/api";
import type { UpdateStatus } from "../lib/api";

// Re-poll cadence for `/api/system/update-status`. The server caches the
// GitHub check for a day, so hourly polls are cheap cache hits that still
// pick up a new release (or a dismissal from another device) promptly.
const POLL_MINUTES = 60;

/**
 * Top-of-app banner shown when `update_available` is true. Dismiss
 * persists by latest_version in server-side app_state (not per-browser
 * localStorage), so acknowledging a release once hides it on every device
 * and matches the TUI's snooze; a newer release re-surfaces it.
 * Polls on mount + at a fixed hourly cadence + on tab
 * visibilitychange. Honors `update_check_mode`: server returns
 * `update_available: false` when mode = off, so nothing renders.
 * Mode = auto also suppresses the banner (the runtime installs
 * silently and the user picks the new binary up next launch).
 * See #984 and #1140.
 */
export function UpdateBanner() {
  const [status, setStatus] = useState<UpdateStatus | null>(null);
  // Optimistic local dismissal so the banner hides immediately on click,
  // before the POST lands and the next poll reflects the persisted value.
  const [locallyDismissed, setLocallyDismissed] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | null = null;

    const poll = async () => {
      const s = await fetchUpdateStatus();
      if (cancelled) return;
      if (s) setStatus(s);
      timer = setTimeout(poll, POLL_MINUTES * 60_000);
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
  // Hide if dismissed for this version, either optimistically this session or
  // persisted server-side (from this or any other device, or the TUI).
  if (locallyDismissed === status.latest_version || status.dismissed_version === status.latest_version) {
    return null;
  }

  const onDismiss = () => {
    if (!status.latest_version) return;
    setLocallyDismissed(status.latest_version);
    void dismissUpdate(status.latest_version);
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
