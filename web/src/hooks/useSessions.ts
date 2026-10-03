import { useCallback, useEffect, useRef, useState } from "react";
import type { SessionResponse } from "../lib/types";
import { fetchSessions, type SessionsEnvelope } from "../lib/api";
import { setServerDown } from "../lib/connectionState";

const POLL_INTERVAL = 3000;
/** How long one poll may stay unanswered before it is written off. Long
 *  enough that a merely slow daemon keeps its place in the queue, short
 *  enough that a lost request costs one gap instead of the rest of the
 *  tab's life. */
const POLL_DEADLINE_MS = 15000;
const LOCAL_ORDERING_WINDOW_MS = 4000;

export function useSessions() {
  const [sessions, setSessions] = useState<SessionResponse[]>([]);
  const [workspaceOrdering, setWorkspaceOrdering] = useState<string[]>([]);
  const [error, setError] = useState(false);
  const [loaded, setLoaded] = useState(false);
  const lastLocalOrderingAtRef = useRef<number>(0);

  const injectSession = useCallback((session: SessionResponse) => {
    // Single-session responses omit rate-limit fields; keep the last known values (#3514).
    setSessions((prev) => {
      if (prev.some((s) => s.id === session.id)) return prev;
      return [session, ...prev];
    });
  }, []);

  const markLocalOrderingUpdate = useCallback(() => {
    lastLocalOrderingAtRef.current = Date.now();
  }, []);

  const applyResult = useCallback((data: SessionsEnvelope | null) => {
    if (data !== null) {
      setSessions(data.sessions);
      // Ignore server ordering while a local drag's PUT may still be landing.
      if (Date.now() - lastLocalOrderingAtRef.current > LOCAL_ORDERING_WINDOW_MS) {
        setWorkspaceOrdering(data.workspace_ordering);
      }
      setError(false);
      setServerDown(false);
    } else {
      setError(true);
      setServerDown(true);
    }
    setLoaded(true);
  }, []);

  useEffect(() => {
    // Recursive setTimeout so polls never overlap: two /api/sessions
    // responses can cross, and the slower one must not roll the canonical
    // list back to an older snapshot.
    //
    // Each request also carries a deadline. Arming the next poll only from
    // the previous response hands the whole cadence to one lost request, so
    // the race is between the answer and the deadline instead: the loser is
    // superseded and its late answer is dropped by the generation check
    // rather than applied when it finally lands.
    let cancelled = false;
    let generation = 0;
    let timer: number | undefined;
    const deadlines = new Set<number>();

    const scheduleNext = () => {
      if (cancelled) return;
      timer = setTimeout(() => void tick(), POLL_INTERVAL);
    };

    const tick = async () => {
      const mine = ++generation;
      const deadline = setTimeout(() => {
        deadlines.delete(deadline);
        if (cancelled || mine !== generation) return;
        // Lost, not answered: supersede it and keep the cadence. At most one
        // superseded request stays in flight alongside its replacement.
        generation += 1;
        scheduleNext();
      }, POLL_DEADLINE_MS);
      deadlines.add(deadline);

      const data = await fetchSessions();
      deadlines.delete(deadline);
      clearTimeout(deadline);
      if (cancelled || mine !== generation) return;
      applyResult(data);
      scheduleNext();
    };

    void tick();
    return () => {
      cancelled = true;
      clearTimeout(timer);
      for (const deadline of deadlines) clearTimeout(deadline);
      deadlines.clear();
    };
  }, [applyResult]);

  const setSessionStatus = useCallback((id: string, status: SessionResponse["status"]) => {
    setSessions((prev) => prev.map((s) => (s.id === id ? { ...s, status } : s)));
  }, []);

  const applySession = useCallback((session: SessionResponse) => {
    setSessions((prev) =>
      prev.map((s) =>
        s.id === session.id
          ? {
              ...session,
              rate_limit: session.rate_limit === undefined ? s.rate_limit : session.rate_limit,
              rate_limit_auto_resume: session.rate_limit_auto_resume ?? s.rate_limit_auto_resume,
            }
          : s,
      ),
    );
  }, []);

  return {
    sessions,
    workspaceOrdering,
    setWorkspaceOrdering,
    markLocalOrderingUpdate,
    error,
    loaded,
    injectSession,
    setSessionStatus,
    applySession,
  };
}
