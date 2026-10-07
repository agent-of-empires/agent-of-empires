import { useCallback, useEffect, useRef, useState } from "react";
import { fetchSystemHealth, type SystemHealth } from "../lib/api";

/** Gap between a reading landing and the next request while the per-agent
 *  detail is open, matching the TUI's status tick. */
const ACTIVE_POLL_GAP = 500;
/** Gap for the collapsed one-line readout, whose percentages move slowly
 *  enough that a faster tick only costs the host work. */
const IDLE_POLL_GAP = 15000;

/** Poll the host health readout while `enabled`.
 *
 *  Every sample costs the daemon a process scan and a tmux pane listing, so
 *  the poll is scaled to what is actually being read: a hidden tab stops
 *  entirely, the collapsed strip ticks slowly, and only the open drill-down
 *  runs at the TUI's rate. A disabled strip polls nothing and reads as absent
 *  regardless of what the last poll saw.
 *
 *  One request at a time, always. The next is scheduled from when the last one
 *  settles rather than on a clock, and the in-flight request is tracked across
 *  restarts, so opening the detail while a sample is pending waits for it
 *  instead of adding a second. This mirrors the TUI, which holds one sample in
 *  flight and asks again when it lands.
 *
 *  CPU is a delta on the server, so the first reading after a daemon start
 *  reports it unknown. */
export function useSystemHealth(enabled: boolean, detailOpen = false): SystemHealth | null {
  const [health, setHealth] = useState<SystemHealth | null>(null);
  const [visible, setVisible] = useState(() => !document.hidden);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  /** The request in flight, shared across effect restarts. */
  const inFlight = useRef<Promise<SystemHealth | null> | null>(null);
  /** Bumped per effect run so a superseded loop stops after its await. */
  const generation = useRef(0);

  const apply = useCallback((next: SystemHealth | null) => {
    // A failed poll keeps the last good reading rather than blanking the
    // strip: the daemon being briefly unreachable is not a health signal.
    if (next) setHealth(next);
  }, []);

  useEffect(() => {
    const onVisibility = () => setVisible(!document.hidden);
    document.addEventListener("visibilitychange", onVisibility);
    return () => document.removeEventListener("visibilitychange", onVisibility);
  }, []);

  useEffect(() => {
    if (!enabled || !visible) return;
    generation.current += 1;
    const mine = generation.current;
    const gap = detailOpen ? ACTIVE_POLL_GAP : IDLE_POLL_GAP;

    const poll = async () => {
      if (generation.current !== mine) return;
      // A sample started by a previous run is still the current reading; wait
      // for it rather than asking the host for a second one alongside it.
      if (inFlight.current) {
        await inFlight.current;
        if (generation.current !== mine) return;
      }
      const request = fetchSystemHealth().catch(() => null);
      inFlight.current = request;
      const next = await request;
      if (inFlight.current === request) inFlight.current = null;
      if (generation.current !== mine) return;
      apply(next);
      timer.current = setTimeout(() => void poll(), gap);
    };
    void poll();

    return () => {
      generation.current += 1;
      if (timer.current) clearTimeout(timer.current);
    };
  }, [enabled, visible, detailOpen, apply]);

  return enabled ? health : null;
}
