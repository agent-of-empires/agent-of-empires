import { useEffect, useRef, useState } from "react";
import { fetchPluginUiState, type PluginUiEntry, type PluginUiNotification } from "../lib/api";
import { reportError, reportInfo } from "../lib/toastBus";

// Polls the host's plugin UI-state snapshot on the same 3s cadence as the
// session list, so a session and its plugin slots refresh in the same window
// (no separate, tearing-prone clock). Notifications are point-in-time: each
// arrives once, tracked by its monotonic seq, and is pushed to the toast bus.
const POLL_INTERVAL = 3000;

/** Map a plugin notification onto the toast bus. The bus only distinguishes
 *  error vs info, so danger/warn tones surface as errors and the rest as info;
 *  the title and optional body are joined into the single-line toast. */
function toast(n: PluginUiNotification): void {
  const message = n.body ? `${n.title}: ${n.body}` : n.title;
  if (n.tone === "danger" || n.tone === "warn") {
    reportError(message);
  } else {
    reportInfo(message);
  }
}

// A poll faster than this shows no refresh indicator: a background fetch that
// settles in tens of milliseconds would otherwise strobe the indicator on and
// off every cadence. Only a poll slow enough to be worth surfacing (network
// latency, a rate-limited GitHub refresh) crosses the threshold and shows.
const REFRESH_INDICATOR_DELAY = 250;

export function usePluginUiState() {
  const [entries, setEntries] = useState<PluginUiEntry[]>([]);
  const [isRefreshing, setIsRefreshing] = useState(false);
  // Highest notification seq already toasted. Seeded from the first snapshot so
  // a page load does not replay the whole backlog as fresh toasts.
  const lastNotifySeqRef = useRef<number | null>(null);

  useEffect(() => {
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | null = null;
    let slowTimer: ReturnType<typeof setTimeout> | null = null;

    const apply = (notifications: PluginUiNotification[]) => {
      const maxSeq = notifications.reduce((m, n) => Math.max(m, n.seq), 0);
      const seen = lastNotifySeqRef.current;
      // Seed on the first snapshot, and re-seed when maxSeq drops below the
      // watermark: the ring is in-memory and dies with the daemon, so after a
      // restart seqs start low again. Treat that as a fresh ring and adopt the
      // current backlog as seen rather than filtering every new toast out.
      if (seen === null || maxSeq < seen) {
        lastNotifySeqRef.current = maxSeq;
        return;
      }
      for (const n of notifications) {
        if (n.seq > seen) toast(n);
      }
      lastNotifySeqRef.current = Math.max(seen, maxSeq);
    };

    // Recursive setTimeout, not setInterval: the next poll is scheduled only
    // after the current one settles, so requests never overlap and a slow
    // response cannot land after a newer one and roll the dashboard back to
    // stale plugin UI. A failed fetch (null) just skips this round.
    const tick = async () => {
      // Flip the indicator on only once the poll outlasts the threshold, so a
      // fast fetch never shows it. Cleared in finally whether the fetch
      // succeeds, returns null, or the threshold never fires.
      slowTimer = setTimeout(() => {
        if (!cancelled) setIsRefreshing(true);
      }, REFRESH_INDICATOR_DELAY);
      try {
        const state = await fetchPluginUiState();
        if (cancelled || state === null) return;
        setEntries(state.entries);
        apply(state.notifications);
      } finally {
        if (slowTimer) clearTimeout(slowTimer);
        if (!cancelled) {
          setIsRefreshing(false);
          timer = setTimeout(() => void tick(), POLL_INTERVAL);
        }
      }
    };
    void tick();
    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
      if (slowTimer) clearTimeout(slowTimer);
    };
  }, []);

  return { entries, isRefreshing };
}
