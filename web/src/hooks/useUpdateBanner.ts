import { useEffect, useState } from "react";
import { dismissUpdate, fetchUpdateStatus } from "../lib/api";
import type { UpdateStatus } from "../lib/api";
import { onServerDownChange } from "../lib/connectionState";

// Re-poll cadence for `/api/system/update-status`.
const POLL_MINUTES = 60;

/** The update status to offer, or `null` when nothing should show. */
export function useUpdateBanner(): { status: UpdateStatus; dismiss: () => void } | null {
  const [status, setStatus] = useState<UpdateStatus | null>(null);
  // Optimistic local dismissal so the banner hides immediately on click,
  // before the POST lands and the next poll reflects the persisted value.
  const [locallyDismissed, setLocallyDismissed] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | null = null;
    let latest = 0;

    // Visibility and reconnect can start a poll while one is in flight. Only the newest applies
    // its answer and schedules the next one, so a slow older reply can't restore a stale offer.
    const poll = async () => {
      const seq = ++latest;
      const s = await fetchUpdateStatus();
      if (cancelled || seq !== latest) return;
      if (s) setStatus(s);
      if (timer) clearTimeout(timer);
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
    // A reconnect may follow the Daemon restarting into the update itself, so the offer could be stale.
    const offServerDown = onServerDownChange((down) => {
      if (down) return;
      if (timer) clearTimeout(timer);
      void poll();
    });

    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
      clearTimeout(initialTimer);
      document.removeEventListener("visibilitychange", onVisibility);
      offServerDown();
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

  const latest = status.latest_version;
  const dismiss = () => {
    setLocallyDismissed(latest);
    void dismissUpdate(latest);
  };
  return { status, dismiss };
}
