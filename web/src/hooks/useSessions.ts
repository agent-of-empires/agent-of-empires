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

interface PushedStatus {
  status: SessionResponse["status"];
  cursor: RuntimeCursor;
}

/** A pushed status wins over a row observation of the same epoch only while it is newer than that observation. */
const outranks = (held: PushedStatus, observed: RuntimeCursor) =>
  held.cursor.epoch === observed.epoch && held.cursor.revision > observed.revision;

function withPushedStatus(row: SessionResponse, observed: RuntimeCursor, held: PushedStatus | undefined) {
  return held && outranks(held, observed) ? { ...row, status: held.status } : row;
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
  // Newest pushed status per row. A push is a partial observation: it is compared with each row's own observation,
  // never with the global floor, and one from an epoch not yet installed waits for a read of that epoch.
  const pushedRef = useRef(new Map<string, PushedStatus>());

  const injectSession = useCallback((session: SessionResponse) => {
    setState((prev) =>
      prev.sessions.some((s) => s.id === session.id) ? prev : { ...prev, sessions: [session, ...prev.sessions] },
    );
  }, []);

  const markLocalOrderingUpdate = useCallback(() => {
    lastLocalOrderingAtRef.current = Date.now();
  }, []);

  const applyResult = useCallback((data: SessionsEnvelope | null) => {
    setLoaded(true);
    if (data !== null) {
      setError(false);
      setServerDown(false);
      const cursor = data.cursor;
      const floor = floorGet.current;
      if (abandonedEpochs.current.has(cursor.epoch)) return;
      if (floor?.epoch === cursor.epoch && cursor.revision < floor.revision) return;
      if (floor && floor.epoch !== cursor.epoch) abandonedEpochs.current.add(floor.epoch);
      floorGet.current = cursor;
      const pushed = pushedRef.current;
      const present = new Set(data.sessions.map((s) => s.id));
      const sessions = data.sessions.map((s) => withPushedStatus(s, cursor, pushed.get(s.id)));
      for (const [id, held] of pushed) {
        const waiting = held.cursor.epoch !== cursor.epoch && !abandonedEpochs.current.has(held.cursor.epoch);
        if (!present.has(id) || !(outranks(held, cursor) || waiting)) pushed.delete(id);
      }
      setState({
        sessions,
        observedById: Object.fromEntries(data.sessions.map((session) => [session.id, cursor])),
        epoch: cursor.epoch,
      });
      // Ignore server ordering while a local drag's PUT may still be landing.
      if (Date.now() - lastLocalOrderingAtRef.current > LOCAL_ORDERING_WINDOW_MS) {
        setWorkspaceOrdering(data.workspace_ordering);
      }
    } else {
      setError(true);
      setServerDown(true);
    }
  }, []);

  // Published after the commit so no row sees the new ordinal beside the previous snapshot.
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

  // Refetches the authoritative list after the stream (re)connected or lost events.
  const refresh = useCallback(() => {
    void fetchSessions().then(applyResult);
  }, [applyResult]);

  // The server sends a move only after publishing a snapshot that includes it, and stamps it with that cursor.
  const applyPushedStatus = useCallback((id: string, status: SessionResponse["status"], cursor: RuntimeCursor) => {
    if (abandonedEpochs.current.has(cursor.epoch)) return;
    const held = pushedRef.current.get(id);
    if (held && held.cursor.epoch === cursor.epoch && held.cursor.revision >= cursor.revision) return;
    pushedRef.current.set(id, { status, cursor });
    setState((prev) => {
      if (prev.epoch !== cursor.epoch) return prev;
      const observed = prev.observedById[id];
      if (observed?.epoch === cursor.epoch && observed.revision >= cursor.revision) return prev;
      return { ...prev, sessions: prev.sessions.map((s) => (s.id === id ? { ...s, status } : s)) };
    });
  }, []);

  const setSessionStatus = useCallback((id: string, status: SessionResponse["status"]) => {
    setState((prev) => ({ ...prev, sessions: prev.sessions.map((s) => (s.id === id ? { ...s, status } : s)) }));
  }, []);

  const clearDeletingStatus = useCallback((selected: SessionResponse[]) => {
    const originals = new Map(selected.map((session) => [session.id, session]));
    setState((previous) => {
      let sessions: SessionResponse[] | undefined;
      for (const [index, row] of previous.sessions.entries()) {
        const original = originals.get(row.id);
        if (
          row.status === "Deleting" &&
          original &&
          row.profile === original.profile &&
          row.created_at === original.created_at &&
          row.lifecycle_generation === original.lifecycle_generation &&
          row.trashed_at === original.trashed_at &&
          row.trash_lifecycle_id === original.trash_lifecycle_id
        ) {
          sessions ??= previous.sessions.slice();
          sessions[index] = { ...row, status: original.status };
        }
      }
      return sessions ? { ...previous, sessions } : previous;
    });
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
        sessions: prev.sessions.map((s) =>
          s.id === session.id
            ? withPushedStatus(mergeSession(session, s), cursor, pushedRef.current.get(session.id))
            : s,
        ),
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
    clearDeletingStatus,
    applySession,
    applySessionMutation,
    refresh,
    applyPushedStatus,
  };
}
