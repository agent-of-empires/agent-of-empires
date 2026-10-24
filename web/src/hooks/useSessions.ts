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
  // Pushed statuses by id. The snapshot behind GET /api/sessions is published after the push is broadcast, so a
  // poll can legitimately still show the old status; a pushed status is held until a poll that began after the push
  // shows it. Equal status alone is not an acknowledgement: an older read can match a newer override.
  const pushedRef = useRef(new Map<string, { status: SessionResponse["status"]; seq: number }>());
  const pushSeq = useRef(0);

  const injectSession = useCallback((session: SessionResponse) => {
    setState((prev) =>
      prev.sessions.some((s) => s.id === session.id) ? prev : { ...prev, sessions: [session, ...prev.sessions] },
    );
  }, []);

  const markLocalOrderingUpdate = useCallback(() => {
    lastLocalOrderingAtRef.current = Date.now();
  }, []);

  const applyResult = useCallback((data: SessionsEnvelope | null, startSeq: number) => {
    setLoaded(true);
    if (data !== null) {
      setError(false);
      setServerDown(false);
      const cursor = data.cursor;
      const floor = floorGet.current;
      if (abandonedEpochs.current.has(cursor.epoch)) return;
      if (floor?.epoch === cursor.epoch && cursor.revision < floor.revision) return;
      if (floor && floor.epoch !== cursor.epoch) {
        abandonedEpochs.current.add(floor.epoch);
        pushedRef.current.clear();
      }
      floorGet.current = cursor;
      const pushed = pushedRef.current;
      const present = new Set(data.sessions.map((s) => s.id));
      for (const id of pushed.keys()) if (!present.has(id)) pushed.delete(id);
      const sessions = data.sessions.map((s) => {
        const held = pushed.get(s.id);
        if (held === undefined) return s;
        if (s.status === held.status && startSeq >= held.seq) {
          pushed.delete(s.id);
          return s;
        }
        return { ...s, status: held.status };
      });
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
      const startSeq = pushSeq.current;
      const request = new AbortController();
      controller = request;
      const requestDeadline = setTimeout(() => {
        if (cancelled || mine !== generation) return;
        generation++;
        request.abort();
        applyResult(null, startSeq);
        scheduleNext();
      }, POLL_DEADLINE_MS);
      deadline = requestDeadline;
      const data = await fetchSessions(request.signal);
      clearTimeout(requestDeadline);
      if (cancelled || mine !== generation) return;
      controller = undefined;
      applyResult(data, startSeq);
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

  // Refetches the authoritative list after the stream (re)connected or lost events. A held status may stand for a
  // transition whose follow-up was missed, and only pushes from this stream onward are recorded again.
  const refresh = useCallback(() => {
    pushedRef.current.clear();
    const startSeq = pushSeq.current;
    void fetchSessions().then((data) => applyResult(data, startSeq));
  }, [applyResult]);

  const applyPushedStatus = useCallback((id: string, status: SessionResponse["status"]) => {
    pushedRef.current.set(id, { status, seq: ++pushSeq.current });
    setState((prev) => ({ ...prev, sessions: prev.sessions.map((s) => (s.id === id ? { ...s, status } : s)) }));
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
    clearDeletingStatus,
    applySession,
    applySessionMutation,
    refresh,
    applyPushedStatus,
  };
}
