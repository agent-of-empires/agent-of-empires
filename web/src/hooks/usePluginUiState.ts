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

export function usePluginUiState() {
  const [entries, setEntries] = useState<PluginUiEntry[]>([]);
  // Highest notification seq already toasted. Seeded from the first snapshot so
  // a page load does not replay the whole backlog as fresh toasts.
  const lastNotifySeqRef = useRef<number | null>(null);
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);

  useEffect(() => {
    let cancelled = false;
    const poll = async () => {
      const state = await fetchPluginUiState();
      if (cancelled || state === null) return;
      setEntries(state.entries);

      const maxSeq = state.notifications.reduce((m, n) => Math.max(m, n.seq), 0);
      if (lastNotifySeqRef.current === null) {
        // First snapshot: adopt the backlog as already-seen, toast nothing.
        lastNotifySeqRef.current = maxSeq;
      } else {
        const seen = lastNotifySeqRef.current;
        for (const n of state.notifications) {
          if (n.seq > seen) toast(n);
        }
        lastNotifySeqRef.current = Math.max(seen, maxSeq);
      }
    };
    void poll();
    intervalRef.current = setInterval(() => void poll(), POLL_INTERVAL);
    return () => {
      cancelled = true;
      if (intervalRef.current) clearInterval(intervalRef.current);
    };
  }, []);

  return entries;
}
