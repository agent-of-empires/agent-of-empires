import { useCallback, useEffect, useRef, useState } from "react";
import type { SessionResponse } from "../lib/types";
import { fetchSessions, type RuntimeCursor, type SessionMutation, type SessionsEnvelope } from "../lib/api";
import { setServerDown } from "../lib/connectionState";

const POLL_INTERVAL = 3000;
const POLL_DEADLINE_MS = 15000;
const LOCAL_ORDERING_WINDOW_MS = 4000;

interface SessionState {
  sessions: SessionResponse[];
  observedById: Record<string, RuntimeCursor>;
  epoch: string | null;
}

function mergeSession(session: SessionResponse, previous: SessionResponse): SessionResponse {
  return {
    ...session,
    rate_limit: session.rate_limit === undefined ? previous.rate_limit : session.rate_limit,
    rate_limit_auto_resume: session.rate_limit_auto_resume ?? previous.rate_limit_auto_resume,
  };
}

export function useSessions() {
  const [state, setState] = useState<SessionState>({ sessions: [], observedById: {}, epoch: null });
  const [workspaceOrdering, setWorkspaceOrdering] = useState<string[]>([]);
  const [error, setError] = useState(false);
  const [loaded, setLoaded] = useState(false);
  const lastLocalOrderingAtRef = useRef<number>(0);
  // GET admission is global; only installed rows attest individual mutation receipts.
  const floorGet = useRef<RuntimeCursor | null>(null);
  const abandonedEpochs = useRef(new Set<string>());

  const injectSession = useCallback((session: SessionResponse) => {
    setState((prev) =>
      prev.sessions.some((s) => s.id === session.id) ? prev : { ...prev, sessions: [session, ...prev.sessions] },
    );
  }, []);

  const markLocalOrderingUpdate = useCallback(() => {
    lastLocalOrderingAtRef.current = Date.now();
  }, []);

  const applyResult = useCallback((data: SessionsEnvelope | null) => {
    if (data !== null) {
      const cursor = data.cursor;
      const floor = floorGet.current;
      if (abandonedEpochs.current.has(cursor.epoch)) return;
      if (floor?.epoch === cursor.epoch && cursor.revision < floor.revision) return;
      if (floor && floor.epoch !== cursor.epoch) abandonedEpochs.current.add(floor.epoch);
      floorGet.current = cursor;
      setState({
        sessions: data.sessions,
        observedById: Object.fromEntries(data.sessions.map((session) => [session.id, cursor])),
        epoch: cursor.epoch,
      });
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
    let cancelled = false;
    let generation = 0;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let deadline: ReturnType<typeof setTimeout> | undefined;
    let controller: AbortController | undefined;
    const scheduleNext = () => {
      if (!cancelled) timer = setTimeout(() => void tick(), POLL_INTERVAL);
    };
    const tick = async () => {
      const mine = ++generation;
      const request = new AbortController();
      controller = request;
      const requestDeadline = setTimeout(() => {
        if (cancelled || mine !== generation) return;
        generation++;
        request.abort();
        applyResult(null);
        scheduleNext();
      }, POLL_DEADLINE_MS);
      deadline = requestDeadline;
      const data = await fetchSessions(request.signal);
      clearTimeout(requestDeadline);
      if (cancelled || mine !== generation) return;
      controller = undefined;
      applyResult(data);
      scheduleNext();
    };
    void tick();
    return () => {
      cancelled = true;
      generation++;
      clearTimeout(timer);
      clearTimeout(deadline);
      controller?.abort();
    };
  }, [applyResult]);

  const setSessionStatus = useCallback((id: string, status: SessionResponse["status"]) => {
    setState((prev) => ({ ...prev, sessions: prev.sessions.map((s) => (s.id === id ? { ...s, status } : s)) }));
  }, []);

  const applySession = useCallback((session: SessionResponse) => {
    setState((prev) => {
      const observedById = { ...prev.observedById };
      delete observedById[session.id];
      return {
        ...prev,
        sessions: prev.sessions.map((s) => (s.id === session.id ? mergeSession(session, s) : s)),
        observedById,
      };
    });
  }, []);

  const applySessionMutation = useCallback(({ session, cursor }: SessionMutation) => {
    const floor = floorGet.current;
    if (abandonedEpochs.current.has(cursor.epoch) || (floor && floor.epoch !== cursor.epoch)) return;
    if (!floor || cursor.revision > floor.revision) floorGet.current = cursor;
    setState((prev) => {
      const observed = prev.observedById[session.id];
      if (observed?.epoch === cursor.epoch && observed.revision >= cursor.revision) return prev;
      // A mutation response cannot resurrect an id absent from the canonical list.
      if (!prev.sessions.some((s) => s.id === session.id)) return prev;
      return {
        sessions: prev.sessions.map((s) => (s.id === session.id ? mergeSession(session, s) : s)),
        observedById: { ...prev.observedById, [session.id]: cursor },
        epoch: cursor.epoch,
      };
    });
  }, []);

  return {
    sessions: state.sessions,
    observedById: state.observedById,
    runtimeEpoch: state.epoch,
    workspaceOrdering,
    setWorkspaceOrdering,
    markLocalOrderingUpdate,
    error,
    loaded,
    injectSession,
    setSessionStatus,
    applySession,
    applySessionMutation,
  };
}
