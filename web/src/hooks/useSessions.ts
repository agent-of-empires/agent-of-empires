import { useCallback, useEffect, useRef, useState } from "react";
import type { SessionResponse } from "../lib/types";
import { fetchSessions, type SessionsEnvelope } from "../lib/api";
import { setServerDown } from "../lib/connectionState";

const POLL_INTERVAL = 3000;
const LOCAL_ORDERING_WINDOW_MS = 4000;

export function useSessions() {
  const [sessions, setSessions] = useState<SessionResponse[]>([]);
  const [workspaceOrdering, setWorkspaceOrdering] = useState<string[]>([]);
  const [error, setError] = useState(false);
  const [loaded, setLoaded] = useState(false);
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);
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
    void fetchSessions().then(applyResult);
    intervalRef.current = setInterval(() => void fetchSessions().then(applyResult), POLL_INTERVAL);
    return () => {
      if (intervalRef.current) clearInterval(intervalRef.current);
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
