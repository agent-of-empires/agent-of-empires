import { useCallback, useEffect, useRef, useState } from "react";
import { fetchPluginUiState, type PluginUiEntry, type PluginUiNotification } from "../lib/api";
import { reportError, reportInfo, reportOpenLink } from "../lib/toastBus";

const POLL_INTERVAL = 3000;
const BOOST_INTERVAL = 500;
const BOOST_MS = 15000;

function toast(n: PluginUiNotification): void {
  const message = n.body ? `${n.title}: ${n.body}` : n.title;
  if (n.href) {
    reportOpenLink(message, n.href);
  } else if (n.tone === "danger" || n.tone === "warn") {
    reportError(message);
  } else {
    reportInfo(message);
  }
}

const REFRESH_INDICATOR_DELAY = 250;

export function usePluginUiState() {
  const [entries, setEntries] = useState<PluginUiEntry[]>([]);
  const [revisions, setRevisions] = useState<Record<string, Record<string, number>>>({});
  const [isRefreshing, setIsRefreshing] = useState(false);
  const lastNotifySeqRef = useRef<number | null>(null);
  const pokeRef = useRef<() => void>(() => {});

  useEffect(() => {
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | null = null;
    let slowTimer: ReturnType<typeof setTimeout> | null = null;
    let inFlight = false;
    let boostUntil = 0;

    const apply = (notifications: PluginUiNotification[]) => {
      const maxSeq = notifications.reduce((m, n) => Math.max(m, n.seq), 0);
      const seen = lastNotifySeqRef.current;
      // Seed on first snapshot, and re-seed after a daemon restart resets seqs.
      if (seen === null || maxSeq < seen) {
        lastNotifySeqRef.current = maxSeq;
        return;
      }
      for (const n of notifications) {
        if (n.seq > seen) toast(n);
      }
      lastNotifySeqRef.current = Math.max(seen, maxSeq);
    };

    const scheduleNext = () => {
      if (cancelled) return;
      const delay = Date.now() < boostUntil ? BOOST_INTERVAL : POLL_INTERVAL;
      timer = setTimeout(() => void tick(), delay);
    };

    // Recursive setTimeout so polls never overlap and a slow response can't roll back state.
    const tick = async () => {
      if (inFlight) return; // a poke during an in-flight fetch; scheduleNext re-fires
      inFlight = true;
      slowTimer = setTimeout(() => {
        if (!cancelled) setIsRefreshing(true);
      }, REFRESH_INDICATOR_DELAY);
      try {
        const state = await fetchPluginUiState();
        if (cancelled || state === null) return;
        setEntries(state.entries);
        setRevisions(state.revisions ?? {});
        apply(state.notifications);
      } finally {
        if (slowTimer) clearTimeout(slowTimer);
        inFlight = false;
        if (!cancelled) {
          setIsRefreshing(false);
          scheduleNext();
        }
      }
    };

    pokeRef.current = () => {
      boostUntil = Date.now() + BOOST_MS;
      if (inFlight) return; // the in-flight fetch's scheduleNext picks up the boost
      if (timer) clearTimeout(timer);
      void tick();
    };

    void tick();
    return () => {
      cancelled = true;
      pokeRef.current = () => {};
      if (timer) clearTimeout(timer);
      if (slowTimer) clearTimeout(slowTimer);
    };
  }, []);

  const poke = useCallback(() => pokeRef.current(), []);

  return { entries, revisions, isRefreshing, poke };
}
