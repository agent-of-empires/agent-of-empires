import { useCallback, useEffect, useRef, useState } from "react";
import type { SessionResponse } from "../lib/types";
import { fetchSessions, type SessionsEnvelope } from "../lib/api";
import { setServerDown } from "../lib/connectionState";

const POLL_INTERVAL = 3000;
// How long after a local drag we treat the client's ordering as
// authoritative. The PUT typically lands in <1s and the poll runs
// every 3s, so 4s is comfortably above both. Past this window the
// server's value wins again, which is how a remote drag on another
// device propagates back to us.
const LOCAL_ORDERING_WINDOW_MS = 4000;

export function useSessions() {
  const [sessions, setSessions] = useState<SessionResponse[]>([]);
  const [workspaceOrdering, setWorkspaceOrdering] = useState<string[]>([]);
  const [error, setError] = useState(false);
  // True once the first fetch attempt resolves (success or failure). Lets
  // callers distinguish "still loading" from "list confirmed empty / no
  // such session," which matters for refresh on `/session/<id>` where an
  // empty `sessions` array during initial paint otherwise indistinguishably
  // collapses to the dashboard fallback. See #1351.
  const [loaded, setLoaded] = useState(false);
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const lastLocalOrderingAtRef = useRef<number>(0);

  const injectSession = useCallback((session: SessionResponse) => {
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
      // Drop the server's ordering while a recent local drag is still
      // settling. Without this guard, a poll that fires between our
      // optimistic setState and the PUT landing can read the file in
      // its pre-drag state and revert the row to its old slot.
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

  const refresh = useCallback(async () => {
    applyResult(await fetchSessions());
  }, [applyResult]);

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

  return {
    sessions,
    workspaceOrdering,
    setWorkspaceOrdering,
    markLocalOrderingUpdate,
    error,
    loaded,
    refresh,
    injectSession,
    setSessionStatus,
  };
}
